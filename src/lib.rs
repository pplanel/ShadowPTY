pub mod input;
pub mod output;
pub mod palette;
pub mod pty_manager;
pub mod rasterizer;
pub mod recorder;
pub mod report;
pub mod screen;
pub mod screenshot;
pub mod server;
pub mod session;
pub mod signals;

// Backwards-compatibility re-exports
pub mod formatter {
    pub use crate::screen::{format_screen, screen_text};
}

pub mod snapshot {
    pub use crate::screen::{
        CursorShape, Screen as Snapshot, ScreenCell as SnapCell, ScreenCursor as SnapCursor,
    };
}
