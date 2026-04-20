use x11rb::connection::Connection;
use x11rb::protocol::screensaver::ConnectionExt as _;
use x11rb::rust_connection::RustConnection;

pub struct IdleMonitor {
    conn: RustConnection,
    root: u32,
}

impl IdleMonitor {
    pub fn new() -> Option<Self> {
        let (conn, screen_num) = x11rb::connect(None).ok()?;
        let root = conn.setup().roots.get(screen_num)?.root;
        conn.screensaver_query_version(1, 0).ok()?.reply().ok()?;
        Some(Self { conn, root })
    }

    pub fn idle_ms(&self) -> Option<u32> {
        let reply = self
            .conn
            .screensaver_query_info(self.root)
            .ok()?
            .reply()
            .ok()?;
        Some(reply.ms_since_user_input)
    }
}
