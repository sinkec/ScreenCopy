//! GTK4/libadwaita user interface.

pub mod about;
mod monitor_picker;
mod recording;
mod section;
pub mod splash;
mod tray;
mod tv;
pub mod window;

/// Also names the icon, the desktop file and the polkit policy.
pub const APP_ID: &str = "io.github.sinkec.MonitorBeam";
