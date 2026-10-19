mod idle;
mod ipc;
mod storage;

use fuzzy_matcher::skim::SkimMatcherV2;
use fuzzy_matcher::FuzzyMatcher;
use iced::keyboard;
use iced::widget::image::Handle;
use iced::widget::operation::{focus, scroll_to};
use iced::widget::scrollable::{AbsoluteOffset, Viewport};
use iced::widget::{
    button, column, container, image as image_widget, opaque, row, scrollable, stack, text,
    text_input, toggler, Space,
};
use iced::{
    alignment, window, Background, Border, Color, Element, Fill, Length, Shadow, Subscription,
    Task, Theme,
};
use image::codecs::png::{PngDecoder, PngEncoder};
use image::{ExtendedColorType, ImageDecoder, ImageEncoder};
use std::borrow::Cow;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::io::{Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::mpsc::{channel, Receiver};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use idle::IdleMonitor;
use storage::{LoadedState, Settings, Storage};

const CONTENT_SCROLL_ID: iced::widget::Id = iced::widget::Id::new("content");
const LIST_SCROLL_ID: iced::widget::Id = iced::widget::Id::new("list");
const SEARCH_INPUT_ID: iced::widget::Id = iced::widget::Id::new("search");
const JUMP_INPUT_ID: iced::widget::Id = iced::widget::Id::new("jump");
const POLL_INTERVAL_MIN_MS: u32 = 100;
const POLL_INTERVAL_MAX_MS: u32 = 10_000;
const IDLE_THRESHOLD_MS: u32 = 30_000;
/// Fixed pixel height reserved per sidebar row. The list is virtualized against
/// this stride, so it must match the actual rendered row height in `clip_row`.
const LIST_ROW_STRIDE: f32 = 40.0;
/// Extra rows rendered above/below the visible window to avoid blanks while scrolling.
const LIST_OVERSCAN: usize = 6;

/// A command received on the control socket, forwarded from the listener thread
/// into the iced event loop by `ipc_worker`.
enum IpcCommand {
    Focus,
    Note(String),
}

static IPC_RX: OnceLock<Mutex<Option<Receiver<IpcCommand>>>> = OnceLock::new();

/// Set from `--minimized`/`--start-minimized` on the command line. When true the
/// main window is minimized the first time it opens, so Clipper can run from a
/// login autostart entry without grabbing focus on every boot.
static START_MINIMIZED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

fn main() -> iced::Result {
    for arg in std::env::args().skip(1) {
        match arg.as_str() {
            "--minimized" | "--start-minimized" => {
                START_MINIMIZED.store(true, std::sync::atomic::Ordering::Relaxed);
            }
            "-h" | "--help" => {
                println!(
                    "clipper — clipboard history\n\n\
                     Usage: clipper [OPTIONS]\n\n\
                     Options:\n\
                     \x20 --minimized, --start-minimized   Start with the window minimized\n\
                     \x20 -h, --help                        Show this help"
                );
                return Ok(());
            }
            other => {
                eprintln!("clipper: unknown option: {other} (try --help)");
                std::process::exit(2);
            }
        }
    }

    if !setup_singleton() {
        return Ok(());
    }

    let icon = load_window_icon()?;

    iced::application(Clipper::new, Clipper::update, Clipper::view)
        .title(Clipper::title)
        .theme(Clipper::theme)
        .subscription(Clipper::subscription)
        .window_size((900.0, 560.0))
        .window(window::Settings {
            icon: Some(icon),
            platform_specific: window::settings::PlatformSpecific {
                application_id: "clipper".to_string(),
                ..Default::default()
            },
            ..window::Settings::default()
        })
        .run()
}

/// Returns `true` if this process should proceed as the primary instance.
/// Returns `false` if another instance is already running and was notified to focus.
fn setup_singleton() -> bool {
    let sock_path = ipc::socket_path();

    if let Ok(mut stream) = UnixStream::connect(&sock_path) {
        let _ = stream.write_all(format!("{}\n", ipc::VERB_FOCUS).as_bytes());
        let _ = stream.flush();
        eprintln!("clipper: another instance is already running; requested focus");
        return false;
    }

    let _ = std::fs::remove_file(&sock_path);

    let listener = match UnixListener::bind(&sock_path) {
        Ok(l) => l,
        Err(err) => {
            eprintln!(
                "clipper: could not bind IPC socket at {:?}: {err}; continuing without singleton",
                sock_path
            );
            return true;
        }
    };

    let (tx, rx) = channel::<IpcCommand>();
    let _ = IPC_RX.set(Mutex::new(Some(rx)));

    std::thread::spawn(move || loop {
        match listener.accept() {
            Ok((mut stream, _)) => {
                // Clients write a short request and close the write half; read to
                // EOF so multi-line note bodies arrive intact.
                let mut buf = Vec::new();
                let _ = stream.read_to_end(&mut buf);
                if tx.send(parse_ipc(&buf)).is_err() {
                    break;
                }
            }
            Err(_) => break,
        }
    });

    true
}

/// Parses a control-socket request: the first line is the verb, everything after
/// the first newline is the payload. Unknown/empty requests fall back to focus,
/// preserving the original single-byte focus handshake.
fn parse_ipc(buf: &[u8]) -> IpcCommand {
    let text = String::from_utf8_lossy(buf);
    let (verb, rest) = match text.split_once('\n') {
        Some((v, r)) => (v.trim(), r),
        None => (text.trim(), ""),
    };
    if verb == ipc::VERB_NOTE {
        IpcCommand::Note(rest.to_string())
    } else {
        IpcCommand::Focus
    }
}

/// Background clipboard poller. Runs on its own OS thread and only forwards a
/// message when the clipboard content hash changes, so an idle session produces
/// no messages — and therefore no UI redraws. Honors the X11 idle threshold and
/// stops itself when the subscription is dropped (e.g. poll interval changed).
fn clipboard_worker(interval_ms: &u64) -> impl iced::futures::Stream<Item = Message> {
    let interval = Duration::from_millis((*interval_ms).max(POLL_INTERVAL_MIN_MS as u64));
    iced::stream::channel(16, move |output: iced::futures::channel::mpsc::Sender<Message>| async move {
        std::thread::spawn(move || {
            let mut output = output;
            let idle_monitor = IdleMonitor::new();
            let mut clipboard = arboard::Clipboard::new().ok();
            let mut last_hash: Option<u64> = None;
            let mut last_idle: Option<bool> = None;

            loop {
                let idle_now = idle_monitor
                    .as_ref()
                    .and_then(|m| m.idle_ms())
                    .map(|ms| ms >= IDLE_THRESHOLD_MS);

                if let Some(idle) = idle_now {
                    if last_idle != Some(idle) {
                        match output.try_send(Message::IdleChanged(idle)) {
                            Ok(()) => last_idle = Some(idle),
                            Err(e) if e.is_disconnected() => return,
                            Err(_) => {}
                        }
                    }
                }

                if !idle_now.unwrap_or(false) {
                    if clipboard.is_none() {
                        clipboard = arboard::Clipboard::new().ok();
                    }
                    if let Some(cb) = clipboard.as_mut() {
                        if let Some((msg, hash)) = read_clipboard(cb) {
                            if last_hash != Some(hash) {
                                match output.try_send(msg) {
                                    Ok(()) => last_hash = Some(hash),
                                    Err(e) if e.is_disconnected() => return,
                                    Err(_) => {}
                                }
                            }
                        }
                    }
                }

                std::thread::sleep(interval);
            }
        });
    })
}

/// Reads the current clipboard, returning a message plus its content hash, or
/// `None` if the clipboard holds nothing we capture. Does not deduplicate — the
/// caller compares the hash against the last value it forwarded.
fn read_clipboard(cb: &mut arboard::Clipboard) -> Option<(Message, u64)> {
    if let Ok(txt) = cb.get_text() {
        if !txt.is_empty() {
            let hash = hash_slice(txt.as_bytes());
            return Some((Message::ClipText(txt, hash), hash));
        }
    }
    if let Ok(img) = cb.get_image() {
        let mut bytes = img.bytes.into_owned();
        fix_zero_alpha(&mut bytes);
        let hash = hash_slice(&bytes);
        return Some((
            Message::ClipImage {
                width: img.width as u32,
                height: img.height as u32,
                rgba: Arc::new(bytes),
                hash,
            },
            hash,
        ));
    }
    None
}

/// Event-driven control-socket handler. The listener thread (see
/// `setup_singleton`) hands us commands through `IPC_RX`: a second `clipper`
/// invocation requests focus, and `clipper-note` injects a note. No polling.
fn ipc_worker() -> impl iced::futures::Stream<Item = Message> {
    iced::stream::channel(4, |output: iced::futures::channel::mpsc::Sender<Message>| async move {
        let rx = IPC_RX
            .get()
            .and_then(|m| m.lock().ok().and_then(|mut g| g.take()));
        if let Some(rx) = rx {
            std::thread::spawn(move || {
                let mut output = output;
                while let Ok(cmd) = rx.recv() {
                    let msg = match cmd {
                        IpcCommand::Focus => Message::FocusRequested,
                        IpcCommand::Note(text) => Message::InjectNote(text),
                    };
                    match output.try_send(msg) {
                        Ok(()) => {}
                        Err(e) if e.is_disconnected() => return,
                        Err(_) => {}
                    }
                }
            });
        }
    })
}

fn load_window_icon() -> Result<window::Icon, iced::Error> {
    let png = include_bytes!("../assets/clipper.png");
    let decoder = PngDecoder::new(std::io::Cursor::new(png))
        .map_err(|err| iced::Error::WindowCreationFailed(Box::new(err)))?;
    let (width, height) = decoder.dimensions();
    let mut rgba = vec![0; decoder.total_bytes() as usize];

    decoder
        .read_image(&mut rgba)
        .map_err(|err| iced::Error::WindowCreationFailed(Box::new(err)))?;

    window::icon::from_rgba(rgba, width, height)
        .map_err(|err| iced::Error::WindowCreationFailed(Box::new(err)))
}

/// Runs a native "Save As" dialog and writes the clip to the chosen path.
/// Returns `Ok(Some(path))` on success, or `Ok(None)` if the user cancelled.
async fn save_clip_as(data: ClipData) -> Result<Option<String>, String> {
    let (default_name, filter_name, extensions): (&str, &str, &[&str]) = match &data {
        ClipData::Text(_) => ("clip.txt", "Text", &["txt"]),
        ClipData::Image { .. } => ("clip.png", "PNG image", &["png"]),
    };

    let handle = rfd::AsyncFileDialog::new()
        .set_file_name(default_name)
        .add_filter(filter_name, extensions)
        .save_file()
        .await;

    let Some(handle) = handle else {
        return Ok(None);
    };
    let path = handle.path().to_path_buf();

    match data {
        ClipData::Text(t) => {
            std::fs::write(&path, t).map_err(|e| e.to_string())?;
        }
        ClipData::Image {
            width,
            height,
            rgba,
        } => {
            let file = std::fs::File::create(&path).map_err(|e| e.to_string())?;
            PngEncoder::new(std::io::BufWriter::new(file))
                .write_image(rgba.as_slice(), width, height, ExtendedColorType::Rgba8)
                .map_err(|e| e.to_string())?;
        }
    }

    Ok(Some(path.display().to_string()))
}

#[derive(Debug, Clone)]
enum ClipData {
    Text(String),
    Image {
        width: u32,
        height: u32,
        rgba: Arc<Vec<u8>>,
    },
}

#[derive(Debug, Clone)]
struct Clip {
    id: u64,
    data: ClipData,
    hash: u64,
    preview: String,
    image_handle: Option<Handle>,
    /// True for clips injected via `clipper-note`; drives the list row accent.
    note: bool,
}

/// Per-row color treatment in the sidebar, derived from the clip's origin/type.
#[derive(Clone, Copy)]
enum RowAccent {
    /// Normal clipboard text — the neutral default.
    Text,
    /// AI note injected via `clipper-note` — green tint.
    Note,
    /// Image clip — violet tint.
    Image,
}

impl RowAccent {
    /// (idle background, idle border, hover background, hover border).
    fn surfaces(self) -> (Color, Color, Color, Color) {
        match self {
            RowAccent::Text => (
                Color::from_rgb8(0x1b, 0x1d, 0x23),
                Color::from_rgb8(0x25, 0x28, 0x31),
                Color::from_rgb8(0x23, 0x27, 0x30),
                Color::from_rgb8(0x2e, 0x33, 0x3e),
            ),
            RowAccent::Note => (
                Color::from_rgb8(0x15, 0x24, 0x1f),
                Color::from_rgb8(0x2c, 0x55, 0x49),
                Color::from_rgb8(0x1b, 0x30, 0x28),
                Color::from_rgb8(0x37, 0x6b, 0x5a),
            ),
            RowAccent::Image => (
                Color::from_rgb8(0x1f, 0x1a, 0x29),
                Color::from_rgb8(0x42, 0x36, 0x60),
                Color::from_rgb8(0x27, 0x20, 0x36),
                Color::from_rgb8(0x52, 0x44, 0x78),
            ),
        }
    }

    /// Preview-text color when the row is not selected (selected rows are white).
    fn label_color(self) -> Color {
        match self {
            RowAccent::Text => Color::from_rgb8(0xc3, 0xc7, 0xd1),
            RowAccent::Note => Color::from_rgb8(0x9c, 0xd9, 0xc4),
            RowAccent::Image => Color::from_rgb8(0xc6, 0xb6, 0xe6),
        }
    }
}

struct Clipper {
    clips: Vec<Clip>,
    selected: Option<u64>,
    next_id: u64,
    settings: Settings,
    storage: Option<Storage>,
    settings_open: bool,
    settings_draft_history_size: String,
    settings_draft_poll_interval_ms: String,
    search_active: bool,
    search_query: String,
    jump_active: bool,
    jump_query: String,
    jump_error: bool,
    main_window: Option<window::Id>,
    is_idle: bool,
    list_offset_y: f32,
    list_viewport_h: f32,
}

#[derive(Debug, Clone)]
enum Message {
    Select(u64),
    Remove(u64),
    CopySelected,
    SaveAsSelected,
    SaveComplete(Result<Option<String>, String>),
    ClipText(String, u64),
    ClipImage {
        width: u32,
        height: u32,
        rgba: Arc<Vec<u8>>,
        hash: u64,
    },
    IdleChanged(bool),
    FocusRequested,
    InjectNote(String),
    ListScrolled {
        offset_y: f32,
        viewport_h: f32,
    },
    ToggleSettings,
    SettingsHistorySizeChanged(String),
    SettingsPollIntervalChanged(String),
    SettingsDiskToggled(bool),
    SettingsMinimizedToggled(bool),
    SettingsDeleteAll,
    SettingsSave,
    WindowOpened(window::Id),
    Key(keyboard::Event),
    SearchChanged(String),
    SearchDismiss,
    JumpQueryChanged(String),
    JumpSubmit,
}

impl Clipper {
    fn new() -> Self {
        let (storage, loaded) = match Storage::open(&storage::default_db_path()) {
            Ok(s) => {
                let loaded = s.load().unwrap_or_default();
                (Some(s), loaded)
            }
            Err(err) => {
                eprintln!("clipper: failed to open storage: {err}");
                (None, LoadedState::default())
            }
        };

        let mut next_id = 0u64;
        let clips: Vec<Clip> = loaded
            .clips
            .into_iter()
            .map(|pc| {
                if pc.id >= next_id {
                    next_id = pc.id + 1;
                }
                let (preview, data, image_handle) = match pc.data {
                    storage::ClipData::Text(t) => {
                        let preview = preview_text(&t);
                        (preview, ClipData::Text(t), None)
                    }
                    storage::ClipData::Image {
                        width,
                        height,
                        rgba,
                    } => {
                        let preview = format!("image · {}×{}", width, height);
                        let handle = Handle::from_rgba(width, height, rgba.clone());
                        (
                            preview,
                            ClipData::Image {
                                width,
                                height,
                                rgba: Arc::new(rgba),
                            },
                            Some(handle),
                        )
                    }
                };
                Clip {
                    id: pc.id,
                    data,
                    hash: pc.hash,
                    preview,
                    image_handle,
                    note: pc.is_note,
                }
            })
            .collect();

        let selected = clips.first().map(|c| c.id);
        let draft_history = loaded.settings.history_size.to_string();
        let draft_poll = loaded.settings.poll_interval_ms.to_string();

        Self {
            clips,
            selected,
            next_id,
            settings: loaded.settings,
            storage,
            settings_open: false,
            settings_draft_history_size: draft_history,
            settings_draft_poll_interval_ms: draft_poll,
            search_active: false,
            search_query: String::new(),
            jump_active: false,
            jump_query: String::new(),
            jump_error: false,
            main_window: None,
            is_idle: false,
            list_offset_y: 0.0,
            // Overestimate until the first scroll event reports the real height;
            // rendering a few extra rows is harmless, rendering too few is not.
            list_viewport_h: 600.0,
        }
    }

    fn update(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::Select(id) => {
                let changed = self.selected != Some(id);
                self.selected = Some(id);
                if changed {
                    return scroll_to(
                        CONTENT_SCROLL_ID.clone(),
                        AbsoluteOffset { x: 0.0, y: 0.0 },
                    );
                }
                Task::none()
            }
            Message::Remove(id) => {
                self.remove_clip(id);
                Task::none()
            }
            Message::CopySelected => {
                self.copy_selected();
                Task::none()
            }
            Message::SaveAsSelected => {
                match self
                    .selected
                    .and_then(|id| self.clips.iter().find(|c| c.id == id))
                {
                    Some(clip) => {
                        Task::perform(save_clip_as(clip.data.clone()), Message::SaveComplete)
                    }
                    None => Task::none(),
                }
            }
            Message::SaveComplete(result) => {
                match result {
                    Ok(Some(path)) => eprintln!("clipper: saved to {path}"),
                    Ok(None) => {} // user cancelled the dialog
                    Err(err) => eprintln!("clipper: save failed: {err}"),
                }
                Task::none()
            }
            Message::ClipText(txt, hash) => {
                self.ingest_text(txt, hash);
                Task::none()
            }
            Message::ClipImage {
                width,
                height,
                rgba,
                hash,
            } => {
                self.ingest_image(width, height, rgba, hash);
                Task::none()
            }
            Message::IdleChanged(idle) => {
                self.is_idle = idle;
                Task::none()
            }
            Message::FocusRequested => {
                if let Some(id) = self.main_window {
                    Task::batch([window::minimize(id, false), window::gain_focus(id)])
                } else {
                    Task::none()
                }
            }
            Message::InjectNote(text) => {
                self.ingest_note(text);
                Task::none()
            }
            Message::ListScrolled {
                offset_y,
                viewport_h,
            } => {
                self.list_offset_y = offset_y;
                self.list_viewport_h = viewport_h;
                Task::none()
            }
            Message::WindowOpened(id) => {
                if self.main_window.is_none() {
                    self.main_window = Some(id);
                    let minimized = self.settings.start_minimized
                        || START_MINIMIZED.load(std::sync::atomic::Ordering::Relaxed);
                    if minimized {
                        return window::minimize(id, true);
                    }
                }
                Task::none()
            }
            Message::Key(event) => {
                if let keyboard::Event::KeyPressed {
                    key, modifiers, ..
                } = event
                {
                    if modifiers.control() {
                        if let keyboard::Key::Character(c) = &key {
                            if c.as_str().eq_ignore_ascii_case("f") {
                                return self.toggle_search();
                            }
                            if c.as_str().eq_ignore_ascii_case("k") {
                                return self.toggle_jump();
                            }
                        }
                    }
                    if matches!(key, keyboard::Key::Named(keyboard::key::Named::Escape)) {
                        if self.jump_active {
                            self.close_jump();
                            return Task::none();
                        }
                        if self.search_active {
                            self.close_search();
                            return scroll_to(
                                LIST_SCROLL_ID.clone(),
                                AbsoluteOffset { x: 0.0, y: 0.0 },
                            );
                        }
                    }
                }
                Task::none()
            }
            Message::SearchChanged(q) => {
                self.search_query = q;
                self.list_offset_y = 0.0;
                scroll_to(LIST_SCROLL_ID.clone(), AbsoluteOffset { x: 0.0, y: 0.0 })
            }
            Message::SearchDismiss => {
                // Unlike Escape (which resets to the top of the full list), this
                // leaves the just-picked result selected and scrolls the now
                // unfiltered list to wherever that clip actually sits.
                self.search_active = false;
                self.search_query.clear();
                match self.selected.and_then(|id| self.clips.iter().position(|c| c.id == id)) {
                    Some(index) => {
                        let row_y = index as f32 * LIST_ROW_STRIDE;
                        scroll_to(LIST_SCROLL_ID.clone(), AbsoluteOffset { x: 0.0, y: row_y })
                    }
                    None => {
                        self.list_offset_y = 0.0;
                        scroll_to(LIST_SCROLL_ID.clone(), AbsoluteOffset { x: 0.0, y: 0.0 })
                    }
                }
            }
            Message::JumpQueryChanged(q) => {
                // Clip ids, so keep only digits; a pasted/typed stray char shouldn't
                // block the parse on submit.
                self.jump_query = q.chars().filter(|c| c.is_ascii_digit()).collect();
                self.jump_error = false;
                Task::none()
            }
            Message::JumpSubmit => {
                let Ok(target) = self.jump_query.trim().parse::<u64>() else {
                    self.jump_error = true;
                    return Task::none();
                };
                match self.clips.iter().position(|c| c.id == target) {
                    Some(index) => {
                        self.close_jump();
                        self.selected = Some(target);
                        let row_y = index as f32 * LIST_ROW_STRIDE;
                        Task::batch([
                            scroll_to(LIST_SCROLL_ID.clone(), AbsoluteOffset { x: 0.0, y: row_y }),
                            scroll_to(
                                CONTENT_SCROLL_ID.clone(),
                                AbsoluteOffset { x: 0.0, y: 0.0 },
                            ),
                        ])
                    }
                    None => {
                        self.jump_error = true;
                        Task::none()
                    }
                }
            }
            Message::ToggleSettings => {
                self.settings_open = !self.settings_open;
                if self.settings_open {
                    self.settings_draft_history_size =
                        self.settings.history_size.to_string();
                    self.settings_draft_poll_interval_ms =
                        self.settings.poll_interval_ms.to_string();
                }
                Task::none()
            }
            Message::SettingsHistorySizeChanged(value) => {
                self.settings_draft_history_size = value;
                Task::none()
            }
            Message::SettingsPollIntervalChanged(value) => {
                self.settings_draft_poll_interval_ms = value;
                Task::none()
            }
            Message::SettingsDiskToggled(enabled) => {
                let was_enabled = self.settings.storage_enabled;
                self.settings.storage_enabled = enabled;
                if let Some(s) = self.storage.as_ref() {
                    let _ = s.save_settings(&self.settings);
                }
                if enabled && !was_enabled {
                    self.flush_all_to_disk();
                }
                Task::none()
            }
            Message::SettingsMinimizedToggled(enabled) => {
                self.settings.start_minimized = enabled;
                if let Some(s) = self.storage.as_ref() {
                    let _ = s.save_settings(&self.settings);
                }
                Task::none()
            }
            Message::SettingsDeleteAll => {
                self.clips.clear();
                self.selected = None;
                if let Some(s) = self.storage.as_ref() {
                    let _ = s.delete_all_clips();
                }
                Task::none()
            }
            Message::SettingsSave => {
                if let Ok(n) = self.settings_draft_history_size.trim().parse::<u32>() {
                    if n > 0 {
                        self.settings.history_size = n;
                        self.trim();
                    }
                }
                if let Ok(n) = self.settings_draft_poll_interval_ms.trim().parse::<u32>() {
                    self.settings.poll_interval_ms =
                        n.clamp(POLL_INTERVAL_MIN_MS, POLL_INTERVAL_MAX_MS);
                }
                self.persist_settings_and_order();
                self.settings_open = false;
                Task::none()
            }
        }
    }

    fn remove_clip(&mut self, id: u64) {
        self.clips.retain(|c| c.id != id);
        if self.selected == Some(id) {
            self.selected = self.clips.first().map(|c| c.id);
        }
        if let Some(s) = self.storage_if_enabled() {
            let _ = s.delete_clip(id);
            let _ = s.save_order(&self.order_vec());
        }
    }

    fn subscription(&self) -> Subscription<Message> {
        Subscription::batch([
            // Clipboard polling runs on a background thread and only emits a
            // message when the clipboard content actually changes, so an idle
            // session produces no messages — and therefore no redraws.
            // Keying the subscription on the interval restarts the worker when
            // the poll interval is changed in settings.
            Subscription::run_with(self.settings.poll_interval_ms as u64, clipboard_worker),
            // Single-instance focus handoff, also event-driven (no timer).
            Subscription::run(ipc_worker),
            window::open_events().map(Message::WindowOpened),
            keyboard::listen().map(Message::Key),
        ])
    }

    fn theme(&self) -> Theme {
        Theme::Dark
    }

    fn title(&self) -> String {
        if self.is_idle {
            "Clipper — Clipboard History (idle)".to_string()
        } else {
            "Clipper — Clipboard History".to_string()
        }
    }

    fn ingest_text(&mut self, txt: String, hash: u64) {
        if let Some(pos) = self.clips.iter().position(|c| c.hash == hash) {
            self.bump_to_top(pos);
            if let Some(s) = self.storage_if_enabled() {
                let _ = s.save_order(&self.order_vec());
            }
            return;
        }
        let preview = preview_text(&txt);
        let id = self.alloc_id();
        if let Some(s) = self.storage_if_enabled() {
            let _ = s.save_clip(id, hash, &storage::ClipData::Text(txt.clone()));
        }
        self.clips.insert(
            0,
            Clip {
                id,
                data: ClipData::Text(txt),
                hash,
                preview,
                image_handle: None,
                note: false,
            },
        );
        if self.selected.is_none() {
            self.selected = Some(id);
        }
        self.trim();
        if let Some(s) = self.storage_if_enabled() {
            let _ = s.save_order(&self.order_vec());
        }
    }

    /// Injects an AI note at the top of the list. Unlike a normal copy this never
    /// touches the system clipboard; the note is hashed in a separate namespace
    /// so it stays distinct from an identical copied-text clip.
    fn ingest_note(&mut self, txt: String) {
        let txt = txt.trim_end_matches('\n').to_string();
        if txt.is_empty() {
            return;
        }
        let hash = note_hash(&txt);
        if let Some(pos) = self.clips.iter().position(|c| c.hash == hash) {
            self.bump_to_top(pos);
            if let Some(s) = self.storage_if_enabled() {
                let _ = s.save_order(&self.order_vec());
            }
            return;
        }
        let preview = preview_text(&txt);
        let id = self.alloc_id();
        if let Some(s) = self.storage_if_enabled() {
            let _ = s.save_note(id, hash, &txt);
        }
        self.clips.insert(
            0,
            Clip {
                id,
                data: ClipData::Text(txt),
                hash,
                preview,
                image_handle: None,
                note: true,
            },
        );
        // Don't yank the user's current selection; only select if nothing is.
        if self.selected.is_none() {
            self.selected = Some(id);
        }
        self.trim();
        if let Some(s) = self.storage_if_enabled() {
            let _ = s.save_order(&self.order_vec());
        }
    }

    fn ingest_image(&mut self, width: u32, height: u32, rgba: Arc<Vec<u8>>, hash: u64) {
        if let Some(pos) = self.clips.iter().position(|c| c.hash == hash) {
            self.bump_to_top(pos);
            if let Some(s) = self.storage_if_enabled() {
                let _ = s.save_order(&self.order_vec());
            }
            return;
        }
        let id = self.alloc_id();
        if let Some(s) = self.storage_if_enabled() {
            let _ = s.save_clip(
                id,
                hash,
                &storage::ClipData::Image {
                    width,
                    height,
                    rgba: (*rgba).clone(),
                },
            );
        }
        let handle = Handle::from_rgba(width, height, (*rgba).clone());
        let preview = format!("image · {}×{}", width, height);
        self.clips.insert(
            0,
            Clip {
                id,
                data: ClipData::Image {
                    width,
                    height,
                    rgba,
                },
                hash,
                preview,
                image_handle: Some(handle),
                note: false,
            },
        );
        if self.selected.is_none() {
            self.selected = Some(id);
        }
        self.trim();
        if let Some(s) = self.storage_if_enabled() {
            let _ = s.save_order(&self.order_vec());
        }
    }

    fn bump_to_top(&mut self, pos: usize) {
        if pos != 0 {
            let clip = self.clips.remove(pos);
            self.clips.insert(0, clip);
        }
    }

    fn alloc_id(&mut self) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    fn trim(&mut self) {
        let max = self.settings.history_size as usize;
        while self.clips.len() > max {
            if let Some(clip) = self.clips.pop() {
                if self.selected == Some(clip.id) {
                    self.selected = self.clips.first().map(|c| c.id);
                }
                if let Some(s) = self.storage_if_enabled() {
                    let _ = s.delete_clip(clip.id);
                }
            }
        }
    }

    fn copy_selected(&mut self) {
        let Some(sel_id) = self.selected else {
            return;
        };
        let Some(pos) = self.clips.iter().position(|c| c.id == sel_id) else {
            return;
        };
        let Ok(mut cb) = arboard::Clipboard::new() else {
            return;
        };
        match &self.clips[pos].data {
            ClipData::Text(t) => {
                let _ = cb.set_text(t.clone());
            }
            ClipData::Image {
                width,
                height,
                rgba,
            } => {
                let img = arboard::ImageData {
                    width: *width as usize,
                    height: *height as usize,
                    bytes: Cow::Borrowed(rgba.as_slice()),
                };
                let _ = cb.set_image(img);
            }
        }
        self.bump_to_top(pos);
        if let Some(s) = self.storage_if_enabled() {
            let _ = s.save_order(&self.order_vec());
        }
    }

    fn order_vec(&self) -> Vec<u64> {
        self.clips.iter().map(|c| c.id).collect()
    }

    fn toggle_search(&mut self) -> Task<Message> {
        self.list_offset_y = 0.0;
        let reset_scroll = scroll_to(LIST_SCROLL_ID.clone(), AbsoluteOffset { x: 0.0, y: 0.0 });
        if self.search_active {
            self.close_search();
            reset_scroll
        } else {
            self.search_active = true;
            Task::batch([focus(SEARCH_INPUT_ID.clone()), reset_scroll])
        }
    }

    fn close_search(&mut self) {
        self.search_active = false;
        self.search_query.clear();
        self.list_offset_y = 0.0;
    }

    /// Ctrl+K: jump straight to a clip by its numeric id. Meant for cases search
    /// can't help with — e.g. you only know the id, or typing a short number beats
    /// typing a search phrase (handy over a mobile/remote session).
    fn toggle_jump(&mut self) -> Task<Message> {
        if self.jump_active {
            self.close_jump();
            Task::none()
        } else {
            self.jump_active = true;
            self.jump_query.clear();
            self.jump_error = false;
            focus(JUMP_INPUT_ID.clone())
        }
    }

    fn close_jump(&mut self) {
        self.jump_active = false;
        self.jump_query.clear();
        self.jump_error = false;
    }

    fn filtered_clips(&self) -> Vec<&Clip> {
        let q = self.search_query.trim();
        if !self.search_active || q.is_empty() {
            return self.clips.iter().collect();
        }
        let matcher = SkimMatcherV2::default();
        let mut scored: Vec<(i64, &Clip)> = self
            .clips
            .iter()
            .filter_map(|c| {
                let haystack: Cow<'_, str> = match &c.data {
                    ClipData::Text(t) => Cow::Borrowed(t.as_str()),
                    ClipData::Image { width, height, .. } => {
                        Cow::Owned(format!("image {}x{}", width, height))
                    }
                };
                matcher.fuzzy_match(&haystack, q).map(|s| (s, c))
            })
            .collect();
        scored.sort_by(|a, b| b.0.cmp(&a.0));
        scored.into_iter().map(|(_, c)| c).collect()
    }

    fn storage_if_enabled(&self) -> Option<&Storage> {
        if self.settings.storage_enabled {
            self.storage.as_ref()
        } else {
            None
        }
    }

    fn persist_settings_and_order(&self) {
        if let Some(s) = self.storage.as_ref() {
            let _ = s.save_settings(&self.settings);
        }
        if let Some(s) = self.storage_if_enabled() {
            let _ = s.save_order(&self.order_vec());
        }
    }

    fn flush_all_to_disk(&self) {
        let Some(s) = &self.storage else { return };
        for clip in &self.clips {
            let data = match &clip.data {
                ClipData::Text(t) => storage::ClipData::Text(t.clone()),
                ClipData::Image {
                    width,
                    height,
                    rgba,
                } => storage::ClipData::Image {
                    width: *width,
                    height: *height,
                    rgba: (**rgba).clone(),
                },
            };
            let _ = s.save_clip(clip.id, clip.hash, &data);
        }
        let _ = s.save_order(&self.order_vec());
    }

    fn view(&self) -> Element<'_, Message> {
        let gear_btn = button(
            container(
                text("⚙")
                    .size(18)
                    .color(Color::from_rgb8(0xc3, 0xc7, 0xd1)),
            )
            .padding([4, 10]),
        )
        .padding(0)
        .on_press(Message::ToggleSettings)
        .style(|_theme: &Theme, status| {
            let bg = match status {
                button::Status::Hovered => Color::from_rgb8(0x2a, 0x2e, 0x38),
                button::Status::Pressed => Color::from_rgb8(0x20, 0x24, 0x2c),
                _ => Color::from_rgb8(0x22, 0x25, 0x2c),
            };
            button::Style {
                background: Some(Background::Color(bg)),
                text_color: Color::WHITE,
                border: Border {
                    color: Color::from_rgb8(0x2e, 0x33, 0x3e),
                    width: 1.0,
                    radius: 6.0.into(),
                },
                shadow: Shadow::default(),
                ..button::Style::default()
            }
        });

        let header = container(
            row![
                text("Clipper")
                    .size(22)
                    .color(Color::from_rgb8(0xe6, 0xe8, 0xef)),
                Space::new().width(Fill),
                gear_btn,
            ]
            .align_y(alignment::Vertical::Center),
        )
        .padding([14, 18])
        .width(Fill)
        .style(|_: &Theme| container::Style {
            background: Some(Background::Color(Color::from_rgb8(0x1a, 0x1c, 0x22))),
            ..container::Style::default()
        });

        let filtered = self.filtered_clips();
        let total = filtered.len();

        // Virtualized list: only build widgets for the rows currently visible
        // (plus a small overscan). Spacers above and below preserve the full
        // scroll height so the scrollbar geometry stays correct. Without this,
        // every redraw built the entire history (up to `history_size` rows),
        // which is what pegged the CPU.
        let first_visible = (self.list_offset_y / LIST_ROW_STRIDE).floor().max(0.0) as usize;
        let window = (self.list_viewport_h / LIST_ROW_STRIDE).ceil() as usize + LIST_OVERSCAN * 2;
        let start = first_visible.saturating_sub(LIST_OVERSCAN).min(total);
        let end = (start + window).min(total);

        let mut list_items = column![].spacing(0);
        if start > 0 {
            list_items = list_items
                .push(Space::new().height(Length::Fixed(start as f32 * LIST_ROW_STRIDE)));
        }
        for clip in &filtered[start..end] {
            let selected = self.selected == Some(clip.id);
            let accent = if clip.note {
                RowAccent::Note
            } else if matches!(clip.data, ClipData::Image { .. }) {
                RowAccent::Image
            } else {
                RowAccent::Text
            };
            list_items = list_items.push(clip_row(clip.id, &clip.preview, selected, accent));
        }
        if end < total {
            list_items = list_items.push(
                Space::new().height(Length::Fixed((total - end) as f32 * LIST_ROW_STRIDE)),
            );
        }

        let mut list_column = column![].spacing(0);
        if self.search_active {
            let search_input = text_input("Search…", &self.search_query)
                .id(SEARCH_INPUT_ID.clone())
                .on_input(Message::SearchChanged)
                .padding(8)
                .size(14);

            // Dismisses search while keeping whatever result you clicked focused
            // in the (now unfiltered) list, instead of snapping back to the top.
            let dismiss_btn = button(
                container(text("✕").size(13).color(Color::from_rgb8(0xc3, 0xc7, 0xd1)))
                    .padding([2, 8]),
            )
            .padding(0)
            .on_press(Message::SearchDismiss)
            .style(|_theme: &Theme, status| {
                let bg = match status {
                    button::Status::Hovered => Color::from_rgb8(0x2a, 0x2e, 0x38),
                    button::Status::Pressed => Color::from_rgb8(0x20, 0x24, 0x2c),
                    _ => Color::from_rgb8(0x22, 0x25, 0x2c),
                };
                button::Style {
                    background: Some(Background::Color(bg)),
                    text_color: Color::WHITE,
                    border: Border {
                        color: Color::from_rgb8(0x2e, 0x33, 0x3e),
                        width: 1.0,
                        radius: 6.0.into(),
                    },
                    shadow: Shadow::default(),
                    ..button::Style::default()
                }
            });

            list_column = list_column.push(
                container(
                    row![search_input, dismiss_btn]
                        .spacing(6)
                        .align_y(alignment::Vertical::Center),
                )
                .padding(8)
                .width(Fill)
                .style(|_: &Theme| container::Style {
                    background: Some(Background::Color(Color::from_rgb8(
                        0x1a, 0x1c, 0x22,
                    ))),
                    border: Border {
                        color: Color::from_rgb8(0x24, 0x27, 0x2f),
                        width: 0.0,
                        radius: 0.0.into(),
                    },
                    ..container::Style::default()
                }),
            );
        }
        list_column = list_column.push(
            scrollable(container(list_items).padding([0, 8]))
                .id(LIST_SCROLL_ID.clone())
                .height(Fill)
                .width(Fill)
                .on_scroll(|vp: Viewport| {
                    let offset = vp.absolute_offset();
                    Message::ListScrolled {
                        offset_y: offset.y,
                        viewport_h: vp.bounds().height,
                    }
                }),
        );

        let list_panel = container(list_column)
            .width(Length::Fixed(300.0))
            .height(Fill)
            .style(|_: &Theme| container::Style {
                background: Some(Background::Color(Color::from_rgb8(0x16, 0x18, 0x1d))),
                border: Border {
                    color: Color::from_rgb8(0x24, 0x27, 0x2f),
                    width: 1.0,
                    radius: 0.0.into(),
                },
                ..container::Style::default()
            });

        let right_panel: Element<'_, Message> = if self.settings_open {
            self.settings_view()
        } else {
            self.content_panel()
        };

        let body = row![list_panel, right_panel].height(Fill);
        let base: Element<'_, Message> =
            column![header, body].width(Fill).height(Fill).into();

        if self.jump_active {
            stack![base, opaque(self.jump_palette())].into()
        } else {
            base
        }
    }

    fn content_panel(&self) -> Element<'_, Message> {
        let selected_clip = self
            .selected
            .and_then(|id| self.clips.iter().find(|c| c.id == id));

        let content_body: Element<'_, Message> = match selected_clip {
            Some(clip) => match &clip.data {
                ClipData::Text(t) => scrollable(
                    container(
                        text(t.clone())
                            .size(15)
                            .color(Color::from_rgb8(0xd6, 0xd9, 0xe0)),
                    )
                    .padding(4),
                )
                .id(CONTENT_SCROLL_ID.clone())
                .width(Fill)
                .height(Fill)
                .into(),
                ClipData::Image { .. } => {
                    let handle = clip
                        .image_handle
                        .clone()
                        .expect("image clip always has handle");
                    scrollable(
                        container(image_widget(handle))
                            .padding(4)
                            .center_x(Fill)
                            .width(Fill),
                    )
                    .id(CONTENT_SCROLL_ID.clone())
                    .width(Fill)
                    .height(Fill)
                    .into()
                }
            },
            None => container(
                text("Waiting for clipboard activity…")
                    .size(15)
                    .color(Color::from_rgb8(0x7a, 0x80, 0x8e)),
            )
            .width(Fill)
            .height(Fill)
            .center_x(Fill)
            .center_y(Fill)
            .into(),
        };

        let meta = match selected_clip {
            Some(clip) => match &clip.data {
                ClipData::Text(t) => format!("text · {} chars", t.chars().count()),
                ClipData::Image { width, height, .. } => {
                    format!("image · {}×{}", width, height)
                }
            },
            None => String::new(),
        };

        let copy_btn = button(
            container(
                text("Copy")
                    .size(13)
                    .color(Color::from_rgb8(0xe6, 0xe8, 0xef)),
            )
            .padding([6, 14]),
        )
        .padding(0)
        .on_press_maybe(selected_clip.map(|_| Message::CopySelected))
        .style(|_theme: &Theme, status| {
            let bg = match status {
                button::Status::Hovered => Color::from_rgb8(0x3b, 0x5b, 0xa9),
                button::Status::Pressed => Color::from_rgb8(0x30, 0x4a, 0x90),
                button::Status::Disabled => Color::from_rgb8(0x23, 0x27, 0x30),
                _ => Color::from_rgb8(0x2b, 0x36, 0x55),
            };
            let border = match status {
                button::Status::Disabled => Color::from_rgb8(0x2e, 0x33, 0x3e),
                _ => Color::from_rgb8(0x5c, 0x82, 0xd6),
            };
            button::Style {
                background: Some(Background::Color(bg)),
                text_color: Color::WHITE,
                border: Border {
                    color: border,
                    width: 1.0,
                    radius: 6.0.into(),
                },
                shadow: Shadow::default(),
                ..button::Style::default()
            }
        });

        let save_as_btn = button(
            container(
                text("Save As")
                    .size(13)
                    .color(Color::from_rgb8(0xe6, 0xe8, 0xef)),
            )
            .padding([6, 14]),
        )
        .padding(0)
        .on_press_maybe(selected_clip.map(|_| Message::SaveAsSelected))
        .style(|_theme: &Theme, status| {
            let bg = match status {
                button::Status::Hovered => Color::from_rgb8(0x3a, 0x41, 0x50),
                button::Status::Pressed => Color::from_rgb8(0x30, 0x36, 0x43),
                button::Status::Disabled => Color::from_rgb8(0x23, 0x27, 0x30),
                _ => Color::from_rgb8(0x2c, 0x31, 0x3d),
            };
            let border = match status {
                button::Status::Disabled => Color::from_rgb8(0x2e, 0x33, 0x3e),
                _ => Color::from_rgb8(0x4a, 0x51, 0x61),
            };
            button::Style {
                background: Some(Background::Color(bg)),
                text_color: Color::WHITE,
                border: Border {
                    color: border,
                    width: 1.0,
                    radius: 6.0.into(),
                },
                shadow: Shadow::default(),
                ..button::Style::default()
            }
        });

        let content_header = row![
            text(meta)
                .size(12)
                .color(Color::from_rgb8(0x7a, 0x80, 0x8e)),
            Space::new().width(Fill),
            save_as_btn,
            copy_btn,
        ]
        .align_y(alignment::Vertical::Center)
        .spacing(10);

        container(
            column![
                content_header,
                Space::new().height(Length::Fixed(10.0)),
                content_body,
            ]
            .padding(18),
        )
        .width(Fill)
        .height(Fill)
        .style(|_: &Theme| container::Style {
            background: Some(Background::Color(Color::from_rgb8(0x1d, 0x20, 0x27))),
            ..container::Style::default()
        })
        .into()
    }

    /// Ctrl+K palette: type a clip id, hit Enter, land on it in the list.
    fn jump_palette(&self) -> Element<'_, Message> {
        let input = text_input("Jump to clip id…", &self.jump_query)
            .id(JUMP_INPUT_ID.clone())
            .on_input(Message::JumpQueryChanged)
            .on_submit(Message::JumpSubmit)
            .padding(10)
            .size(16);

        let mut panel = column![
            text("Jump to clip").size(14).color(Color::from_rgb8(0x9a, 0xa0, 0xad)),
            input,
        ]
        .spacing(8)
        .width(Length::Fixed(280.0));

        if self.jump_error {
            panel = panel.push(
                text("No clip with that id").size(12).color(Color::from_rgb8(0xe0, 0x7a, 0x7a)),
            );
        }

        let card = container(panel)
            .padding(16)
            .style(|_: &Theme| container::Style {
                background: Some(Background::Color(Color::from_rgb8(0x1e, 0x21, 0x28))),
                border: Border {
                    color: Color::from_rgb8(0x3a, 0x3f, 0x4c),
                    width: 1.0,
                    radius: 8.0.into(),
                },
                ..container::Style::default()
            });

        container(card)
            .width(Fill)
            .height(Fill)
            .align_x(alignment::Horizontal::Center)
            .padding([80, 0])
            .style(|_: &Theme| container::Style {
                background: Some(Background::Color(Color {
                    a: 0.55,
                    ..Color::BLACK
                })),
                ..container::Style::default()
            })
            .into()
    }

    fn settings_view(&self) -> Element<'_, Message> {
        let save_btn = button(
            container(
                text("Save")
                    .size(13)
                    .color(Color::from_rgb8(0xe6, 0xe8, 0xef)),
            )
            .padding([6, 14]),
        )
        .padding(0)
        .on_press(Message::SettingsSave)
        .style(|_theme: &Theme, status| {
            let bg = match status {
                button::Status::Hovered => Color::from_rgb8(0x3b, 0x5b, 0xa9),
                button::Status::Pressed => Color::from_rgb8(0x30, 0x4a, 0x90),
                _ => Color::from_rgb8(0x2b, 0x36, 0x55),
            };
            button::Style {
                background: Some(Background::Color(bg)),
                text_color: Color::WHITE,
                border: Border {
                    color: Color::from_rgb8(0x5c, 0x82, 0xd6),
                    width: 1.0,
                    radius: 6.0.into(),
                },
                shadow: Shadow::default(),
                ..button::Style::default()
            }
        });

        let cancel_btn = button(
            container(
                text("Cancel")
                    .size(13)
                    .color(Color::from_rgb8(0xc3, 0xc7, 0xd1)),
            )
            .padding([6, 14]),
        )
        .padding(0)
        .on_press(Message::ToggleSettings)
        .style(|_theme: &Theme, status| {
            let bg = match status {
                button::Status::Hovered => Color::from_rgb8(0x2a, 0x2e, 0x38),
                _ => Color::from_rgb8(0x22, 0x25, 0x2c),
            };
            button::Style {
                background: Some(Background::Color(bg)),
                text_color: Color::WHITE,
                border: Border {
                    color: Color::from_rgb8(0x2e, 0x33, 0x3e),
                    width: 1.0,
                    radius: 6.0.into(),
                },
                shadow: Shadow::default(),
                ..button::Style::default()
            }
        });

        let history_row = row![
            text("History size")
                .size(14)
                .color(Color::from_rgb8(0xc3, 0xc7, 0xd1))
                .width(Length::Fixed(130.0)),
            text_input("1000", &self.settings_draft_history_size)
                .on_input(Message::SettingsHistorySizeChanged)
                .padding(8)
                .size(14)
                .width(Length::Fixed(140.0)),
        ]
        .align_y(alignment::Vertical::Center)
        .spacing(10);

        let history_hint = text("Max number of clips to keep in history.")
            .size(12)
            .color(Color::from_rgb8(0x7a, 0x80, 0x8e));

        let poll_row = row![
            text("Poll interval")
                .size(14)
                .color(Color::from_rgb8(0xc3, 0xc7, 0xd1))
                .width(Length::Fixed(130.0)),
            text_input("500", &self.settings_draft_poll_interval_ms)
                .on_input(Message::SettingsPollIntervalChanged)
                .padding(8)
                .size(14)
                .width(Length::Fixed(100.0)),
            text("ms")
                .size(13)
                .color(Color::from_rgb8(0x7a, 0x80, 0x8e)),
        ]
        .align_y(alignment::Vertical::Center)
        .spacing(8);

        let poll_hint = text(format!(
            "How often to read the clipboard ({}–{} ms).",
            POLL_INTERVAL_MIN_MS, POLL_INTERVAL_MAX_MS
        ))
        .size(12)
        .color(Color::from_rgb8(0x7a, 0x80, 0x8e));

        let disk_row = row![
            text("Persist to disk")
                .size(14)
                .color(Color::from_rgb8(0xc3, 0xc7, 0xd1))
                .width(Length::Fixed(130.0)),
            toggler(self.settings.storage_enabled)
                .on_toggle(Message::SettingsDiskToggled)
                .size(22),
        ]
        .align_y(alignment::Vertical::Center)
        .spacing(10);

        let disk_hint = text("Store clipboard history across restarts.")
            .size(12)
            .color(Color::from_rgb8(0x7a, 0x80, 0x8e));

        let minimized_row = row![
            text("Launch minimized")
                .size(14)
                .color(Color::from_rgb8(0xc3, 0xc7, 0xd1))
                .width(Length::Fixed(130.0)),
            toggler(self.settings.start_minimized)
                .on_toggle(Message::SettingsMinimizedToggled)
                .size(22),
        ]
        .align_y(alignment::Vertical::Center)
        .spacing(10);

        let minimized_hint =
            text("Start with the window minimized — handy when Clipper runs on login.")
                .size(12)
                .color(Color::from_rgb8(0x7a, 0x80, 0x8e));

        let delete_all_btn = button(
            container(
                text("Delete All")
                    .size(13)
                    .color(Color::from_rgb8(0xff, 0xff, 0xff)),
            )
            .padding([6, 14]),
        )
        .padding(0)
        .on_press(Message::SettingsDeleteAll)
        .style(|_theme: &Theme, status| {
            let bg = match status {
                button::Status::Hovered => Color::from_rgb8(0xb5, 0x3a, 0x3a),
                button::Status::Pressed => Color::from_rgb8(0x8f, 0x28, 0x28),
                _ => Color::from_rgb8(0x7a, 0x2a, 0x2a),
            };
            button::Style {
                background: Some(Background::Color(bg)),
                text_color: Color::WHITE,
                border: Border {
                    color: Color::from_rgb8(0xd8, 0x5a, 0x5a),
                    width: 1.0,
                    radius: 6.0.into(),
                },
                shadow: Shadow::default(),
                ..button::Style::default()
            }
        });

        let delete_hint = text("Remove every clip from history (and disk).")
            .size(12)
            .color(Color::from_rgb8(0x7a, 0x80, 0x8e));

        container(
            scrollable(
                column![
                    text("Settings")
                        .size(18)
                        .color(Color::from_rgb8(0xe6, 0xe8, 0xef)),
                    Space::new().height(Length::Fixed(18.0)),
                    history_row,
                    Space::new().height(Length::Fixed(4.0)),
                    history_hint,
                    Space::new().height(Length::Fixed(20.0)),
                    poll_row,
                    Space::new().height(Length::Fixed(4.0)),
                    poll_hint,
                    Space::new().height(Length::Fixed(20.0)),
                    disk_row,
                    Space::new().height(Length::Fixed(4.0)),
                    disk_hint,
                    Space::new().height(Length::Fixed(20.0)),
                    minimized_row,
                    Space::new().height(Length::Fixed(4.0)),
                    minimized_hint,
                    Space::new().height(Length::Fixed(24.0)),
                    row![save_btn, cancel_btn].spacing(10),
                    Space::new().height(Length::Fixed(32.0)),
                    text("Danger zone")
                        .size(14)
                        .color(Color::from_rgb8(0xd8, 0x5a, 0x5a)),
                    Space::new().height(Length::Fixed(10.0)),
                    delete_all_btn,
                    Space::new().height(Length::Fixed(4.0)),
                    delete_hint,
                ]
                .padding(24),
            )
            .width(Fill)
            .height(Fill),
        )
        .width(Fill)
        .height(Fill)
        .style(|_: &Theme| container::Style {
            background: Some(Background::Color(Color::from_rgb8(0x1d, 0x20, 0x27))),
            ..container::Style::default()
        })
        .into()
    }
}

fn clip_row(id: u64, preview: &str, selected: bool, accent: RowAccent) -> Element<'_, Message> {
    let label = text(preview.to_string())
        .size(14)
        // Single line: rows must stay a fixed height for list virtualization.
        // Overlong previews are clipped (see `.clip(true)` below), not wrapped.
        .wrapping(iced::widget::text::Wrapping::None)
        .color(if selected {
            Color::from_rgb8(0xff, 0xff, 0xff)
        } else {
            accent.label_color()
        });

    let select_btn = button(container(label).padding([8, 10]).width(Fill).clip(true))
        .width(Fill)
        .padding(0)
        .on_press(Message::Select(id))
        .style(move |_theme: &Theme, status| {
            let (idle_bg, idle_border, hover_bg, hover_border) = accent.surfaces();
            let (bg, border) = match (selected, status) {
                (true, _) => (
                    Color::from_rgb8(0x3b, 0x5b, 0xa9),
                    Color::from_rgb8(0x5c, 0x82, 0xd6),
                ),
                (false, button::Status::Hovered) => (hover_bg, hover_border),
                (false, _) => (idle_bg, idle_border),
            };
            button::Style {
                background: Some(Background::Color(bg)),
                text_color: Color::WHITE,
                border: Border {
                    color: border,
                    width: 1.0,
                    radius: 6.0.into(),
                },
                shadow: Shadow::default(),
                ..button::Style::default()
            }
        });

    let remove_btn = button(
        container(text("×").size(18).color(Color::from_rgb8(0xd6, 0xd9, 0xe0))).padding([4, 10]),
    )
    .padding(0)
    .on_press(Message::Remove(id))
    .style(|_theme: &Theme, status| {
        let (bg, border) = match status {
            button::Status::Hovered => (
                Color::from_rgb8(0xb5, 0x3a, 0x3a),
                Color::from_rgb8(0xd8, 0x5a, 0x5a),
            ),
            button::Status::Pressed => (
                Color::from_rgb8(0x8f, 0x28, 0x28),
                Color::from_rgb8(0xb5, 0x3a, 0x3a),
            ),
            _ => (
                Color::from_rgb8(0x1b, 0x1d, 0x23),
                Color::from_rgb8(0x25, 0x28, 0x31),
            ),
        };
        button::Style {
            background: Some(Background::Color(bg)),
            text_color: Color::WHITE,
            border: Border {
                color: border,
                width: 1.0,
                radius: 6.0.into(),
            },
            shadow: Shadow::default(),
            ..button::Style::default()
        }
    });

    container(
        row![select_btn, remove_btn]
            .spacing(4)
            .align_y(alignment::Vertical::Center),
    )
    // Fixed cell height so the list can be virtualized against LIST_ROW_STRIDE.
    .center_y(Length::Fixed(LIST_ROW_STRIDE))
    .into()
}

fn fix_zero_alpha(rgba: &mut [u8]) {
    if rgba.chunks_exact(4).all(|p| p[3] == 0) {
        for p in rgba.chunks_exact_mut(4) {
            p[3] = 255;
        }
    }
}

fn hash_slice(bytes: &[u8]) -> u64 {
    let mut h = DefaultHasher::new();
    bytes.hash(&mut h);
    h.finish()
}

/// Hashes note text in its own namespace so an AI note and an identical copied
/// text clip don't collide (and dedupe into each other) in the history.
fn note_hash(text: &str) -> u64 {
    let mut h = DefaultHasher::new();
    "note\0".hash(&mut h);
    text.hash(&mut h);
    h.finish()
}

fn preview_text(t: &str) -> String {
    let first = t.lines().next().unwrap_or("").trim();
    if first.is_empty() {
        return "(blank)".to_string();
    }
    let char_count = first.chars().count();
    if char_count > 64 {
        let truncated: String = first.chars().take(63).collect();
        format!("{}…", truncated)
    } else {
        first.to_string()
    }
}
