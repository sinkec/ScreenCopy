//! The icon in the system tray: the way back to a closed window, the way to end
//! the program, and a visible sign that the screen is being shared.

use std::sync::mpsc::{self, Receiver, Sender};

use ksni::blocking::{Handle, TrayMethods};
use ksni::menu::{MenuItem, StandardItem};
use tracing::{debug, warn};

use super::APP_ID;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrayCommand {
    Show,
    StopSharing,
    Quit,
}

#[derive(Debug)]
struct TrayIcon {
    sharing: bool,
    commands: Sender<TrayCommand>,
}

impl TrayIcon {
    fn send(&self, command: TrayCommand) {
        if self.commands.send(command).is_err() {
            debug!("nobody is listening to the tray");
        }
    }

    fn item(label: &str, command: TrayCommand) -> MenuItem<Self> {
        StandardItem {
            label: label.to_owned(),
            activate: Box::new(move |tray: &mut Self| tray.send(command)),
            ..StandardItem::default()
        }
        .into()
    }
}

impl ksni::Tray for TrayIcon {
    fn id(&self) -> String {
        APP_ID.to_owned()
    }

    fn title(&self) -> String {
        if self.sharing {
            "MonitorBeam: sharing the screen".to_owned()
        } else {
            "MonitorBeam".to_owned()
        }
    }

    fn icon_name(&self) -> String {
        if self.sharing {
            format!("{APP_ID}-sharing")
        } else {
            APP_ID.to_owned()
        }
    }

    /// An installed program finds its icons in the system's theme.
    fn icon_theme_path(&self) -> String {
        if cfg!(debug_assertions) {
            concat!(env!("CARGO_MANIFEST_DIR"), "/data/icons").to_owned()
        } else {
            String::new()
        }
    }

    fn activate(&mut self, _x: i32, _y: i32) {
        self.send(TrayCommand::Show);
    }

    fn menu(&self) -> Vec<MenuItem<Self>> {
        let mut items = vec![Self::item("Open MonitorBeam", TrayCommand::Show)];
        if self.sharing {
            items.push(Self::item("Stop Sharing", TrayCommand::StopSharing));
        }
        items.push(MenuItem::Separator);
        items.push(Self::item("Quit", TrayCommand::Quit));
        items
    }
}

/// The running tray icon.
pub struct Tray {
    handle: Handle<TrayIcon>,
}

impl Tray {
    /// `None` on a desktop without a tray, where closing the window must really
    /// end the program, or it could never be reached again.
    pub fn spawn() -> Option<(Self, Receiver<TrayCommand>)> {
        let (commands, receiver) = mpsc::channel();
        let icon = TrayIcon {
            sharing: false,
            commands,
        };
        match icon.spawn() {
            Ok(handle) => Some((Self { handle }, receiver)),
            Err(error) => {
                warn!(%error, "no system tray available");
                None
            }
        }
    }

    pub fn set_sharing(&self, sharing: bool) {
        if self.handle.update(|icon| icon.sharing = sharing).is_none() {
            debug!("the tray icon has stopped");
        }
    }
}

#[cfg(test)]
mod tests {
    use ksni::Tray as _;

    use super::*;

    fn icon(sharing: bool) -> (TrayIcon, Receiver<TrayCommand>) {
        let (commands, receiver) = mpsc::channel();
        (TrayIcon { sharing, commands }, receiver)
    }

    fn labels(icon: &TrayIcon) -> Vec<String> {
        icon.menu()
            .iter()
            .filter_map(|item| match item {
                MenuItem::Standard(item) => Some(item.label.clone()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn stop_is_offered_only_while_sharing() {
        assert_eq!(labels(&icon(false).0), ["Open MonitorBeam", "Quit"]);
        assert_eq!(
            labels(&icon(true).0),
            ["Open MonitorBeam", "Stop Sharing", "Quit"]
        );
    }

    #[test]
    fn the_icon_shows_that_the_screen_is_being_shared() {
        assert_ne!(icon(true).0.icon_name(), icon(false).0.icon_name());
        assert!(icon(true).0.title().contains("sharing"));
        assert!(!icon(false).0.title().contains("sharing"));
    }

    #[test]
    fn a_click_on_the_icon_opens_the_window() {
        let (mut icon, receiver) = icon(false);
        icon.activate(0, 0);
        assert_eq!(receiver.try_recv(), Ok(TrayCommand::Show));
    }
}
