use redb::{Database, TableDefinition};
use std::path::{Path, PathBuf};

const CLIPS: TableDefinition<u64, &[u8]> = TableDefinition::new("clips");
const META: TableDefinition<&str, &[u8]> = TableDefinition::new("meta");
const ORDER_KEY: &str = "order";
const SETTINGS_KEY: &str = "settings";

const TAG_TEXT: u8 = 1;
const TAG_IMAGE: u8 = 2;

pub struct Storage {
    db: Database,
}

#[derive(Debug, Default)]
pub struct LoadedState {
    pub clips: Vec<PersistedClip>,
    pub settings: Settings,
}

#[derive(Debug)]
pub struct PersistedClip {
    pub id: u64,
    pub hash: u64,
    pub data: ClipData,
}

#[derive(Debug)]
pub enum ClipData {
    Text(String),
    Image {
        width: u32,
        height: u32,
        rgba: Vec<u8>,
    },
}

#[derive(Debug, Clone, Copy)]
pub struct Settings {
    pub history_size: u32,
    pub storage_enabled: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            history_size: 1000,
            storage_enabled: true,
        }
    }
}

impl Storage {
    pub fn open(path: &Path) -> Result<Self, redb::Error> {
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let db = Database::create(path)?;
        let write = db.begin_write()?;
        {
            let _ = write.open_table(CLIPS)?;
            let _ = write.open_table(META)?;
        }
        write.commit()?;
        Ok(Self { db })
    }

    pub fn load(&self) -> Result<LoadedState, redb::Error> {
        let read = self.db.begin_read()?;
        let clips_table = read.open_table(CLIPS)?;
        let meta_table = read.open_table(META)?;

        let settings = match meta_table.get(SETTINGS_KEY)? {
            Some(v) => decode_settings(v.value()).unwrap_or_default(),
            None => Settings::default(),
        };

        let order: Vec<u64> = match meta_table.get(ORDER_KEY)? {
            Some(v) => decode_order(v.value()).unwrap_or_default(),
            None => Vec::new(),
        };

        let mut clips = Vec::new();
        for id in order {
            if let Some(v) = clips_table.get(id)? {
                if let Some(clip) = decode_clip(id, v.value()) {
                    clips.push(clip);
                }
            }
        }

        Ok(LoadedState { clips, settings })
    }

    pub fn save_clip(&self, id: u64, hash: u64, data: &ClipData) -> Result<(), redb::Error> {
        let encoded = encode_clip(hash, data);
        let write = self.db.begin_write()?;
        {
            let mut table = write.open_table(CLIPS)?;
            table.insert(id, encoded.as_slice())?;
        }
        write.commit()?;
        Ok(())
    }

    pub fn delete_clip(&self, id: u64) -> Result<(), redb::Error> {
        let write = self.db.begin_write()?;
        {
            let mut table = write.open_table(CLIPS)?;
            let _ = table.remove(id)?;
        }
        write.commit()?;
        Ok(())
    }

    pub fn save_order(&self, order: &[u64]) -> Result<(), redb::Error> {
        let encoded = encode_order(order);
        let write = self.db.begin_write()?;
        {
            let mut table = write.open_table(META)?;
            table.insert(ORDER_KEY, encoded.as_slice())?;
        }
        write.commit()?;
        Ok(())
    }

    pub fn delete_all_clips(&self) -> Result<(), redb::Error> {
        let write = self.db.begin_write()?;
        write.delete_table(CLIPS)?;
        {
            let _ = write.open_table(CLIPS)?;
            let mut meta = write.open_table(META)?;
            let empty = encode_order(&[]);
            meta.insert(ORDER_KEY, empty.as_slice())?;
        }
        write.commit()?;
        Ok(())
    }

    pub fn save_settings(&self, settings: &Settings) -> Result<(), redb::Error> {
        let encoded = encode_settings(settings);
        let write = self.db.begin_write()?;
        {
            let mut table = write.open_table(META)?;
            table.insert(SETTINGS_KEY, encoded.as_slice())?;
        }
        write.commit()?;
        Ok(())
    }
}

pub fn default_db_path() -> PathBuf {
    let base = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            let home = std::env::var_os("HOME").unwrap_or_default();
            PathBuf::from(home).join(".local/share")
        });
    base.join("clipper").join("clips.redb")
}

fn encode_clip(hash: u64, data: &ClipData) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&hash.to_le_bytes());
    match data {
        ClipData::Text(t) => {
            out.push(TAG_TEXT);
            let bytes = t.as_bytes();
            out.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
            out.extend_from_slice(bytes);
        }
        ClipData::Image { width, height, rgba } => {
            out.push(TAG_IMAGE);
            out.extend_from_slice(&width.to_le_bytes());
            out.extend_from_slice(&height.to_le_bytes());
            out.extend_from_slice(&(rgba.len() as u32).to_le_bytes());
            out.extend_from_slice(rgba);
        }
    }
    out
}

fn decode_clip(id: u64, bytes: &[u8]) -> Option<PersistedClip> {
    if bytes.len() < 9 {
        return None;
    }
    let hash = u64::from_le_bytes(bytes[0..8].try_into().ok()?);
    let tag = bytes[8];
    let rest = &bytes[9..];
    let data = match tag {
        TAG_TEXT => {
            if rest.len() < 4 {
                return None;
            }
            let len = u32::from_le_bytes(rest[0..4].try_into().ok()?) as usize;
            if rest.len() < 4 + len {
                return None;
            }
            let s = std::str::from_utf8(&rest[4..4 + len]).ok()?.to_string();
            ClipData::Text(s)
        }
        TAG_IMAGE => {
            if rest.len() < 12 {
                return None;
            }
            let width = u32::from_le_bytes(rest[0..4].try_into().ok()?);
            let height = u32::from_le_bytes(rest[4..8].try_into().ok()?);
            let rgba_len = u32::from_le_bytes(rest[8..12].try_into().ok()?) as usize;
            if rest.len() < 12 + rgba_len {
                return None;
            }
            let rgba = rest[12..12 + rgba_len].to_vec();
            ClipData::Image {
                width,
                height,
                rgba,
            }
        }
        _ => return None,
    };
    Some(PersistedClip { id, hash, data })
}

fn encode_order(order: &[u64]) -> Vec<u8> {
    let mut out = Vec::with_capacity(4 + order.len() * 8);
    out.extend_from_slice(&(order.len() as u32).to_le_bytes());
    for id in order {
        out.extend_from_slice(&id.to_le_bytes());
    }
    out
}

fn decode_order(bytes: &[u8]) -> Option<Vec<u64>> {
    if bytes.len() < 4 {
        return None;
    }
    let count = u32::from_le_bytes(bytes[0..4].try_into().ok()?) as usize;
    if bytes.len() < 4 + count * 8 {
        return None;
    }
    let mut out = Vec::with_capacity(count);
    for i in 0..count {
        let start = 4 + i * 8;
        out.push(u64::from_le_bytes(bytes[start..start + 8].try_into().ok()?));
    }
    Some(out)
}

fn encode_settings(s: &Settings) -> Vec<u8> {
    let mut out = Vec::with_capacity(5);
    out.extend_from_slice(&s.history_size.to_le_bytes());
    out.push(if s.storage_enabled { 1 } else { 0 });
    out
}

fn decode_settings(bytes: &[u8]) -> Option<Settings> {
    if bytes.len() < 4 {
        return None;
    }
    let history_size = u32::from_le_bytes(bytes[0..4].try_into().ok()?);
    let storage_enabled = if bytes.len() >= 5 { bytes[4] != 0 } else { true };
    Some(Settings {
        history_size,
        storage_enabled,
    })
}
