use iced::widget::image::Handle;
use iced::widget::{button, column, container, image, row, scrollable, text, Space};
use iced::{
    alignment, time, Background, Border, Color, Element, Fill, Length, Shadow, Subscription,
    Theme,
};
use std::borrow::Cow;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::sync::Arc;
use std::time::Duration;

const MAX_HISTORY: usize = 200;
const POLL_INTERVAL: Duration = Duration::from_millis(500);

fn main() -> iced::Result {
    iced::application(Clipper::new, Clipper::update, Clipper::view)
        .title("Clipper — Clipboard History")
        .theme(Clipper::theme)
        .subscription(Clipper::subscription)
        .window_size((900.0, 560.0))
        .run()
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
}

#[derive(Debug, Clone)]
enum Message {
    Select(u64),
    Remove(u64),
    CopySelected,
    Tick,
}

impl Clipper {
    fn new() -> Self {
        Self {
            clips: Vec::new(),
            selected: None,
            next_id: 0,
            last_clipboard_hash: None,
        }
    }

    fn update(&mut self, message: Message) {
        match message {
            Message::Select(id) => self.selected = Some(id),
            Message::Remove(id) => self.remove_clip(id),
            Message::CopySelected => self.copy_selected(),
            Message::Tick => self.poll_clipboard(),
        }
    }

    fn remove_clip(&mut self, id: u64) {
        self.clips.retain(|c| c.id != id);
        if self.selected == Some(id) {
            self.selected = self.clips.first().map(|c| c.id);
        }
    }

    fn subscription(&self) -> Subscription<Message> {
        time::every(POLL_INTERVAL).map(|_| Message::Tick)
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
            return;
        }
        let preview = preview_text(&txt);
        let id = self.alloc_id();
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
    }

    fn ingest_image(&mut self, width: u32, height: u32, bytes: Vec<u8>, hash: u64) {
        if let Some(pos) = self.clips.iter().position(|c| c.hash == hash) {
            self.bump_to_top(pos);
            return;
        }
        let rgba = Arc::new(bytes);
        let handle = Handle::from_rgba(width, height, (*rgba).clone());
        let preview = format!("image · {}×{}", width, height);
        let id = self.alloc_id();
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
        if self.clips.len() > MAX_HISTORY {
            self.clips.truncate(MAX_HISTORY);
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
    }

    fn view(&self) -> Element<'_, Message> {
        let header = container(
            text("Clipper")
                .size(22)
                .color(Color::from_rgb8(0xe6, 0xe8, 0xef)),
        )
        .padding([14, 18])
        .width(Fill)
        .style(|_: &Theme| container::Style {
            background: Some(Background::Color(Color::from_rgb8(0x1a, 0x1c, 0x22))),
            ..container::Style::default()
        });

        let list_items = self
            .clips
            .iter()
            .fold(column![].spacing(4), |col, clip| {
                let selected = self.selected == Some(clip.id);
                col.push(clip_row(clip.id, &clip.preview, selected))
            });

        let list_panel = container(
            scrollable(container(list_items).padding(8))
                .height(Fill)
                .width(Fill),
        )
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
                .width(Fill)
                .height(Fill)
                .into(),
                ClipData::Image { .. } => {
                    let handle = clip
                        .image_handle
                        .clone()
                        .expect("image clip always has handle");
                    scrollable(
                        container(image(handle))
                            .padding(4)
                            .center_x(Fill)
                            .width(Fill),
                    )
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

        let content_panel = container(
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
        });

        let body = row![list_panel, content_panel].height(Fill);
        column![header, body].width(Fill).height(Fill).into()
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
        container(text("×").size(18).color(Color::from_rgb8(0xd6, 0xd9, 0xe0)))
            .padding([4, 10]),
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
    // Some X11 clipboards deliver images with alpha=0 for every pixel,
    // which renders as fully transparent. If every pixel is alpha=0,
    // assume opaque and force alpha to 255.
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
