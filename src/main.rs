mod storage;

use fuzzy_matcher::skim::SkimMatcherV2;
use fuzzy_matcher::FuzzyMatcher;
use iced::keyboard;
use iced::widget::image::Handle;
use iced::widget::operation::{focus, scroll_to};
use iced::widget::scrollable::AbsoluteOffset;
use iced::widget::{
    button, column, container, image as image_widget, row, scrollable, text, text_input, toggler,
    Space,
};
use iced::{
    alignment, time, window, Background, Border, Color, Element, Fill, Length, Shadow,
    Subscription, Task, Theme,
};
use image::codecs::png::PngDecoder;
use image::ImageDecoder;
use std::borrow::Cow;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::io::{Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::mpsc::{channel, Receiver};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use storage::{LoadedState, Settings, Storage};

const CONTENT_SCROLL_ID: iced::widget::Id = iced::widget::Id::new("content");
const SEARCH_INPUT_ID: iced::widget::Id = iced::widget::Id::new("search");
const POLL_INTERVAL_MIN_MS: u32 = 100;
const POLL_INTERVAL_MAX_MS: u32 = 10_000;

static FOCUS_RX: OnceLock<Mutex<Option<Receiver<()>>>> = OnceLock::new();

fn main() -> iced::Result {
    if !setup_singleton() {
        return Ok(());
    }

    let icon = load_window_icon()?;

    iced::application(Clipper::new, Clipper::update, Clipper::view)
        .title("Clipper — Clipboard History")
        .theme(Clipper::theme)
        .subscription(Clipper::subscription)
        .window_size((900.0, 560.0))
        .window(window::Settings {
            icon: Some(icon),
            ..window::Settings::default()
        })
        .run()
}

fn socket_path() -> PathBuf {
    if let Ok(dir) = std::env::var("XDG_RUNTIME_DIR") {
        return PathBuf::from(dir).join("clipper.sock");
    }
    let user = std::env::var("USER").unwrap_or_else(|_| "default".to_string());
    PathBuf::from(format!("/tmp/clipper-{user}.sock"))
}

/// Returns `true` if this process should proceed as the primary instance.
/// Returns `false` if another instance is already running and was notified to focus.
fn setup_singleton() -> bool {
    let sock_path = socket_path();

    if let Ok(mut stream) = UnixStream::connect(&sock_path) {
        let _ = stream.write_all(b"focus\n");
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

    let (tx, rx) = channel::<()>();
    let _ = FOCUS_RX.set(Mutex::new(Some(rx)));

    std::thread::spawn(move || loop {
        match listener.accept() {
            Ok((mut stream, _)) => {
                let mut buf = [0u8; 16];
                let _ = stream.read(&mut buf);
                if tx.send(()).is_err() {
                    break;
                }
            }
            Err(_) => break,
        }
    });

    true
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
}

struct Clipper {
    clips: Vec<Clip>,
    selected: Option<u64>,
    next_id: u64,
    last_clipboard_hash: Option<u64>,
    settings: Settings,
    storage: Option<Storage>,
    settings_open: bool,
    settings_draft_history_size: String,
    settings_draft_poll_interval_ms: String,
    search_active: bool,
    search_query: String,
    main_window: Option<window::Id>,
    focus_rx: Option<Receiver<()>>,
}

#[derive(Debug, Clone)]
enum Message {
    Select(u64),
    Remove(u64),
    CopySelected,
    Tick,
    ToggleSettings,
    SettingsHistorySizeChanged(String),
    SettingsPollIntervalChanged(String),
    SettingsDiskToggled(bool),
    SettingsDeleteAll,
    SettingsSave,
    WindowOpened(window::Id),
    Key(keyboard::Event),
    SearchChanged(String),
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
                }
            })
            .collect();

        let selected = clips.first().map(|c| c.id);
        let draft_history = loaded.settings.history_size.to_string();
        let draft_poll = loaded.settings.poll_interval_ms.to_string();
        let focus_rx = FOCUS_RX
            .get()
            .and_then(|m| m.lock().ok().and_then(|mut g| g.take()));

        Self {
            clips,
            selected,
            next_id,
            last_clipboard_hash: None,
            settings: loaded.settings,
            storage,
            settings_open: false,
            settings_draft_history_size: draft_history,
            settings_draft_poll_interval_ms: draft_poll,
            search_active: false,
            search_query: String::new(),
            main_window: None,
            focus_rx,
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
            Message::Tick => {
                self.poll_clipboard();
                if let (Some(rx), Some(id)) = (&self.focus_rx, self.main_window) {
                    if rx.try_recv().is_ok() {
                        while rx.try_recv().is_ok() {}
                        return Task::batch([
                            window::minimize(id, false),
                            window::gain_focus(id),
                        ]);
                    }
                }
                Task::none()
            }
            Message::WindowOpened(id) => {
                if self.main_window.is_none() {
                    self.main_window = Some(id);
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
                        }
                    }
                    if matches!(key, keyboard::Key::Named(keyboard::key::Named::Escape))
                        && self.search_active
                    {
                        self.close_search();
                    }
                }
                Task::none()
            }
            Message::SearchChanged(q) => {
                self.search_query = q;
                Task::none()
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
            Message::SettingsDeleteAll => {
                self.clips.clear();
                self.selected = None;
                self.last_clipboard_hash = None;
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
        let interval = Duration::from_millis(self.settings.poll_interval_ms as u64);
        Subscription::batch([
            time::every(interval).map(|_| Message::Tick),
            window::open_events().map(Message::WindowOpened),
            keyboard::listen().map(Message::Key),
        ])
    }

    fn theme(&self) -> Theme {
        Theme::Dark
    }

    fn poll_clipboard(&mut self) {
        let Ok(mut cb) = arboard::Clipboard::new() else {
            return;
        };

        if let Ok(txt) = cb.get_text() {
            if !txt.is_empty() {
                let hash = hash_slice(txt.as_bytes());
                if self.last_clipboard_hash != Some(hash) {
                    self.last_clipboard_hash = Some(hash);
                    self.ingest_text(txt, hash);
                }
                return;
            }
        }

        if let Ok(img) = cb.get_image() {
            let mut bytes = img.bytes.into_owned();
            fix_zero_alpha(&mut bytes);
            let hash = hash_slice(&bytes);
            if self.last_clipboard_hash != Some(hash) {
                self.last_clipboard_hash = Some(hash);
                self.ingest_image(img.width as u32, img.height as u32, bytes, hash);
            }
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

    fn ingest_image(&mut self, width: u32, height: u32, bytes: Vec<u8>, hash: u64) {
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
                    rgba: bytes.clone(),
                },
            );
        }
        let rgba = Arc::new(bytes);
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
        let hash = self.clips[pos].hash;
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
        self.last_clipboard_hash = Some(hash);
        self.bump_to_top(pos);
        if let Some(s) = self.storage_if_enabled() {
            let _ = s.save_order(&self.order_vec());
        }
    }

    fn order_vec(&self) -> Vec<u64> {
        self.clips.iter().map(|c| c.id).collect()
    }

    fn toggle_search(&mut self) -> Task<Message> {
        if self.search_active {
            self.close_search();
            Task::none()
        } else {
            self.search_active = true;
            focus(SEARCH_INPUT_ID.clone())
        }
    }

    fn close_search(&mut self) {
        self.search_active = false;
        self.search_query.clear();
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
        let list_items = filtered.iter().fold(column![].spacing(4), |col, clip| {
            let selected = self.selected == Some(clip.id);
            col.push(clip_row(clip.id, &clip.preview, selected))
        });

        let mut list_column = column![].spacing(0);
        if self.search_active {
            let search_input = text_input("Search…", &self.search_query)
                .id(SEARCH_INPUT_ID.clone())
                .on_input(Message::SearchChanged)
                .padding(8)
                .size(14);
            list_column = list_column.push(
                container(search_input)
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
            scrollable(container(list_items).padding(8))
                .height(Fill)
                .width(Fill),
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
        column![header, body].width(Fill).height(Fill).into()
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

        let content_header = row![
            text(meta)
                .size(12)
                .color(Color::from_rgb8(0x7a, 0x80, 0x8e)),
            Space::new().width(Fill),
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

fn clip_row(id: u64, preview: &str, selected: bool) -> Element<'_, Message> {
    let label = text(preview.to_string()).size(14).color(if selected {
        Color::from_rgb8(0xff, 0xff, 0xff)
    } else {
        Color::from_rgb8(0xc3, 0xc7, 0xd1)
    });

    let select_btn = button(container(label).padding([8, 10]).width(Fill))
        .width(Fill)
        .padding(0)
        .on_press(Message::Select(id))
        .style(move |_theme: &Theme, status| {
            let (bg, border) = match (selected, status) {
                (true, _) => (
                    Color::from_rgb8(0x3b, 0x5b, 0xa9),
                    Color::from_rgb8(0x5c, 0x82, 0xd6),
                ),
                (false, button::Status::Hovered) => (
                    Color::from_rgb8(0x23, 0x27, 0x30),
                    Color::from_rgb8(0x2e, 0x33, 0x3e),
                ),
                (false, _) => (
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

    row![select_btn, remove_btn]
        .spacing(4)
        .align_y(alignment::Vertical::Center)
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
