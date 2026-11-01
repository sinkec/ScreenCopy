//! Monitor discovery through the X11 RandR extension.

use std::fmt;

use thiserror::Error;
use tracing::{debug, warn};
use x11rb::connection::Connection;
use x11rb::errors::{ConnectError, ConnectionError, ReplyError};
use x11rb::protocol::randr::ConnectionExt as _;
use x11rb::protocol::xproto::ConnectionExt as _;

/// RandR 1.5 introduced the monitor list used here.
const RANDR_VERSION: (u32, u32) = (1, 5);

#[derive(Debug, Error)]
pub enum MonitorError {
    #[error("cannot connect to the X server: {0}")]
    Connect(#[from] ConnectError),
    #[error("lost the connection to the X server: {0}")]
    Connection(#[from] ConnectionError),
    #[error("the X server rejected a request: {0}")]
    Reply(#[from] ReplyError),
    #[error("the X server has no screen number {0}")]
    NoScreen(usize),
    #[error("monitor has an unusable geometry: {width}x{height}+{x}+{y}")]
    InvalidGeometry {
        x: i16,
        y: i16,
        width: u16,
        height: u16,
    },
}

/// A monitor whose geometry is known to lie inside the X screen's positive
/// coordinates and to be non-empty, so it can always be captured.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Monitor {
    name: String,
    x: u32,
    y: u32,
    width: u32,
    height: u32,
    primary: bool,
}

/// Inclusive pixel bounds, as expected by `ximagesrc` properties
/// `startx`, `starty`, `endx` and `endy`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CaptureRegion {
    pub start_x: u32,
    pub start_y: u32,
    pub end_x: u32,
    pub end_y: u32,
}

impl fmt::Display for CaptureRegion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "startx={} starty={} endx={} endy={}",
            self.start_x, self.start_y, self.end_x, self.end_y
        )
    }
}

impl CaptureRegion {
    pub fn width(&self) -> u32 {
        self.end_x - self.start_x + 1
    }

    pub fn height(&self) -> u32 {
        self.end_y - self.start_y + 1
    }

    /// Drops the last column and row if needed, because 4:2:0 video, which
    /// H.264 encoders and Miracast require, cannot have odd dimensions.
    pub fn with_even_size(self) -> Self {
        Self {
            end_x: self.end_x - self.width() % 2,
            end_y: self.end_y - self.height() % 2,
            ..self
        }
    }
}

impl Monitor {
    pub fn new(
        name: String,
        x: i16,
        y: i16,
        width: u16,
        height: u16,
        primary: bool,
    ) -> Result<Self, MonitorError> {
        let invalid = MonitorError::InvalidGeometry {
            x,
            y,
            width,
            height,
        };
        if width == 0 || height == 0 {
            return Err(invalid);
        }
        let (Ok(x), Ok(y)) = (u32::try_from(x), u32::try_from(y)) else {
            return Err(invalid);
        };
        Ok(Self {
            name,
            x,
            y,
            width: u32::from(width),
            height: u32::from(height),
            primary,
        })
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn is_primary(&self) -> bool {
        self.primary
    }

    pub fn resolution(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    pub fn position(&self) -> (u32, u32) {
        (self.x, self.y)
    }

    /// The region covering exactly this monitor. Cannot overflow because the
    /// values originate from `i16` and `u16`, and width and height are non-zero.
    pub fn capture_region(&self) -> CaptureRegion {
        CaptureRegion {
            start_x: self.x,
            start_y: self.y,
            end_x: self.x + self.width - 1,
            end_y: self.y + self.height - 1,
        }
    }
}

/// Reads the active monitors from the X server named by `$DISPLAY`.
/// Monitors with an unusable geometry are logged and skipped.
pub fn list_monitors() -> Result<Vec<Monitor>, MonitorError> {
    let (conn, screen) = x11rb::connect(None)?;
    let root = conn
        .setup()
        .roots
        .get(screen)
        .ok_or(MonitorError::NoScreen(screen))?
        .root;

    conn.randr_query_version(RANDR_VERSION.0, RANDR_VERSION.1)?
        .reply()?;
    let reply = conn.randr_get_monitors(root, true)?.reply()?;

    let mut monitors = Vec::with_capacity(reply.monitors.len());
    for info in reply.monitors {
        let name = conn.get_atom_name(info.name)?.reply()?.name;
        let name = String::from_utf8_lossy(&name).into_owned();
        match Monitor::new(name, info.x, info.y, info.width, info.height, info.primary) {
            Ok(monitor) => {
                debug!(?monitor, "found monitor");
                monitors.push(monitor);
            }
            Err(error) => warn!(%error, "skipping monitor"),
        }
    }
    Ok(monitors)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn region(x: i16, y: i16, width: u16, height: u16) -> CaptureRegion {
        Monitor::new("M".to_owned(), x, y, width, height, false)
            .unwrap()
            .capture_region()
    }

    fn expected(start_x: u32, start_y: u32, end_x: u32, end_y: u32) -> CaptureRegion {
        CaptureRegion {
            start_x,
            start_y,
            end_x,
            end_y,
        }
    }

    #[test]
    fn landscape_monitor_with_y_offset() {
        assert_eq!(region(0, 142, 2560, 1440), expected(0, 142, 2559, 1581));
    }

    #[test]
    fn portrait_monitor() {
        assert_eq!(region(2560, 0, 1080, 1920), expected(2560, 0, 3639, 1919));
    }

    #[test]
    fn monitor_with_both_offsets() {
        assert_eq!(
            region(3640, 840, 1920, 1080),
            expected(3640, 840, 5559, 1919)
        );
    }

    #[test]
    fn largest_geometry_does_not_overflow() {
        assert_eq!(
            region(i16::MAX, i16::MAX, u16::MAX, u16::MAX),
            expected(32767, 32767, 98301, 98301)
        );
    }

    #[test]
    fn single_pixel_monitor() {
        assert_eq!(region(5, 7, 1, 1), expected(5, 7, 5, 7));
    }

    #[test]
    fn rejects_unusable_geometry() {
        for (x, y, width, height) in [
            (0, 0, 0, 1080),
            (0, 0, 1920, 0),
            (-1, 0, 1920, 1080),
            (0, -1, 1920, 1080),
        ] {
            assert!(matches!(
                Monitor::new("M".to_owned(), x, y, width, height, false),
                Err(MonitorError::InvalidGeometry { .. })
            ));
        }
    }

    #[test]
    fn region_size_is_inclusive() {
        let region = region(3640, 840, 1920, 1080);
        assert_eq!((region.width(), region.height()), (1920, 1080));
    }

    #[test]
    fn even_size_keeps_even_region() {
        let region = region(0, 142, 2560, 1440);
        assert_eq!(region.with_even_size(), region);
    }

    #[test]
    fn even_size_trims_odd_region() {
        let even = region(10, 20, 1367, 769).with_even_size();
        assert_eq!((even.width(), even.height()), (1366, 768));
        assert_eq!((even.start_x, even.start_y), (10, 20));
    }

    #[test]
    fn region_display_names_ximagesrc_properties() {
        assert_eq!(
            expected(1, 2, 3, 4).to_string(),
            "startx=1 starty=2 endx=3 endy=4"
        );
    }
}
