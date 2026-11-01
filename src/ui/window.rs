//! Main window: monitor list and selection.

use std::cell::RefCell;
use std::rc::Rc;

use adw::prelude::*;
use gtk::{gio, glib};
use tracing::{debug, error};

use super::recording::{Recording, RecordingWidgets};
use super::tv::{TvPanel, TvWidgets};
use super::{APP_ID, about};
use crate::audio;
use crate::capture::monitor::{self, Monitor, MonitorError};
use crate::firewall;

const PAGE_MONITORS: &str = "monitors";
const PAGE_MESSAGE: &str = "message";

/// The monitor list and the state behind it.
struct MonitorPicker {
    stack: gtk::Stack,
    page: adw::PreferencesPage,
    list: gtk::ListBox,
    message: adw::StatusPage,
    selected: Rc<RefCell<Option<Monitor>>>,
}

impl MonitorPicker {
    fn new() -> Self {
        let list = gtk::ListBox::builder()
            .selection_mode(gtk::SelectionMode::None)
            .css_classes(["boxed-list"])
            .build();

        let group = adw::PreferencesGroup::builder()
            .title("Monitor")
            .description("Choose the monitor to show on the TV.")
            .build();
        group.add(&list);

        let page = adw::PreferencesPage::new();
        page.add(&group);

        let message = adw::StatusPage::builder()
            .icon_name("video-display-symbolic")
            .build();

        let stack = gtk::Stack::new();
        stack.add_named(&page, Some(PAGE_MONITORS));
        stack.add_named(&message, Some(PAGE_MESSAGE));

        Self {
            stack,
            page,
            list,
            message,
            selected: Rc::default(),
        }
    }

    /// Reads the monitors again, keeping the current selection if it still exists.
    fn refresh(&self) {
        self.show(monitor::list_monitors());
    }

    fn show(&self, result: Result<Vec<Monitor>, MonitorError>) {
        self.list.remove_all();
        let monitors = match result {
            Ok(monitors) if monitors.is_empty() => {
                self.selected.replace(None);
                self.message.set_title("No Monitors Found");
                self.message.set_description(None);
                self.stack.set_visible_child_name(PAGE_MESSAGE);
                return;
            }
            Ok(monitors) => monitors,
            Err(err) => {
                error!(error = %err, "cannot read monitors");
                self.selected.replace(None);
                self.message.set_title("Cannot Read Monitors");
                // The status page renders its description as markup.
                let description = glib::markup_escape_text(&err.to_string());
                self.message.set_description(Some(&description));
                self.stack.set_visible_child_name(PAGE_MESSAGE);
                return;
            }
        };

        let previous = self.selected.borrow().clone();
        let preferred =
            preferred_monitor(&monitors, previous.as_ref().map(Monitor::name)).map(str::to_owned);
        let mut group: Option<gtk::CheckButton> = None;
        for monitor in monitors {
            let is_preferred = preferred.as_deref() == Some(monitor.name());
            let check = self.add_row(monitor);
            check.set_group(group.as_ref());
            if is_preferred {
                check.set_active(true);
            }
            group.get_or_insert(check);
        }
        self.stack.set_visible_child_name(PAGE_MONITORS);
    }

    fn add_row(&self, monitor: Monitor) -> gtk::CheckButton {
        let check = gtk::CheckButton::builder()
            .valign(gtk::Align::Center)
            .build();
        // Monitor names come from the X server, so they are not markup.
        let row = adw::ActionRow::builder()
            .use_markup(false)
            .title(monitor.name())
            .subtitle(row_subtitle(&monitor))
            .activatable_widget(&check)
            .build();
        row.add_prefix(&check);
        self.list.append(&row);

        let selected = Rc::clone(&self.selected);
        check.connect_toggled(move |check| {
            if check.is_active() {
                let region = monitor.capture_region();
                debug!(monitor = monitor.name(), %region, "monitor selected");
                selected.replace(Some(monitor.clone()));
            }
        });
        check
    }
}

/// The previously selected monitor if it still exists, then the primary
/// monitor, then the first one.
fn preferred_monitor<'a>(monitors: &'a [Monitor], previous: Option<&str>) -> Option<&'a str> {
    monitors
        .iter()
        .find(|monitor| Some(monitor.name()) == previous)
        .or_else(|| monitors.iter().find(|monitor| monitor.is_primary()))
        .or_else(|| monitors.first())
        .map(Monitor::name)
}

fn row_subtitle(monitor: &Monitor) -> String {
    let (width, height) = monitor.resolution();
    let (x, y) = monitor.position();
    let primary = if monitor.is_primary() {
        " · Primary"
    } else {
        ""
    };
    format!("{width} × {height} · Position {x}, {y}{primary}")
}

/// Puts the sound back after a session that ended without cleaning up.
fn restore_audio(toasts: &adw::ToastOverlay) {
    let toasts = toasts.clone();
    glib::spawn_future_local(async move {
        if matches!(gio::spawn_blocking(audio::restore_stale).await, Ok(true)) {
            let title = "Restored the sound settings after an interrupted session.";
            toasts.add_toast(adw::Toast::builder().title(title).use_markup(false).build());
        }
    });
}

/// Removes firewall rules that an interrupted session left behind.
fn restore_firewall(toasts: &adw::ToastOverlay) {
    if !firewall::has_stale_rules() {
        return;
    }
    let toasts = toasts.clone();
    glib::spawn_future_local(async move {
        // The helper may wait for the administrator password.
        let title = match gio::spawn_blocking(firewall::restore_stale).await {
            Ok(Ok(())) => "Removed firewall rules left by an interrupted session.".to_owned(),
            Ok(Err(err)) => {
                error!(error = %err, "cannot remove stale firewall rules");
                format!("Firewall rules from an interrupted session remain: {err}")
            }
            Err(_) => {
                error!("firewall cleanup thread failed");
                "Firewall rules from an interrupted session remain.".to_owned()
            }
        };
        toasts.add_toast(adw::Toast::builder().title(title).use_markup(false).build());
    });
}

/// Makes the icon available when running from the source tree. An installed
/// program finds it in the system's icon theme.
pub fn prepare_icons() {
    gtk::Window::set_default_icon_name(APP_ID);
    #[cfg(debug_assertions)]
    if let Some(display) = gtk::gdk::Display::default() {
        gtk::IconTheme::for_display(&display)
            .add_search_path(concat!(env!("CARGO_MANIFEST_DIR"), "/data/icons"));
    }
}

pub fn build(app: &adw::Application) -> adw::ApplicationWindow {
    let picker = MonitorPicker::new();
    picker.refresh();

    let refresh = gtk::Button::builder()
        .icon_name("view-refresh-symbolic")
        .tooltip_text("Refresh Monitors")
        .build();

    let menu = gio::Menu::new();
    menu.append(Some("About ScreenCopy"), Some("win.about"));
    let menu_button = gtk::MenuButton::builder()
        .icon_name("open-menu-symbolic")
        .tooltip_text("Main Menu")
        .menu_model(&menu)
        .build();

    let header = adw::HeaderBar::new();
    header.pack_start(&refresh);
    header.pack_end(&menu_button);

    // Monitor names come from the X server, so they are not markup.
    let banner = adw::Banner::builder().use_markup(false).build();

    let content = adw::ToolbarView::new();
    content.add_top_bar(&header);
    content.add_top_bar(&banner);
    content.set_content(Some(&picker.stack));

    let toasts = adw::ToastOverlay::new();
    toasts.set_child(Some(&content));
    restore_firewall(&toasts);
    restore_audio(&toasts);

    let window = adw::ApplicationWindow::builder()
        .application(app)
        .title(about::NAME)
        .icon_name(APP_ID)
        .default_width(520)
        .default_height(640)
        .content(&toasts)
        .build();

    let about_action = gio::ActionEntry::builder("about")
        .activate(|window: &adw::ApplicationWindow, _, _| about::show_dialog(window))
        .build();
    window.add_action_entries([about_action]);

    // Sharing and recording exclude each other, and neither allows changing
    // the monitor while running.
    let tv_group = adw::PreferencesGroup::new();
    let recording_group = adw::PreferencesGroup::new();
    let monitor_controls: [gtk::Widget; 2] =
        [picker.list.clone().upcast(), refresh.clone().upcast()];

    let share_banner = adw::Banner::builder().use_markup(false).build();
    content.add_top_bar(&share_banner);
    let mut locked_by_sharing = monitor_controls.to_vec();
    locked_by_sharing.push(recording_group.clone().upcast());
    TvPanel::attach(
        TvWidgets {
            group: tv_group.clone(),
            window: window.clone(),
            banner: share_banner,
            toasts: toasts.clone(),
            locked: locked_by_sharing,
        },
        Rc::clone(&picker.selected),
    );

    let mut locked_by_recording = monitor_controls.to_vec();
    locked_by_recording.push(tv_group.clone().upcast());
    Recording::attach(
        RecordingWidgets {
            group: recording_group.clone(),
            window: window.clone(),
            banner,
            toasts,
            locked: locked_by_recording,
        },
        Rc::clone(&picker.selected),
    );
    picker.page.add(&tv_group);
    picker.page.add(&recording_group);

    refresh.connect_clicked(move |_| picker.refresh());

    window
}

#[cfg(test)]
mod tests {
    use super::*;

    fn monitor(name: &str, primary: bool) -> Monitor {
        Monitor::new(name.to_owned(), 0, 142, 2560, 1440, primary).unwrap()
    }

    #[test]
    fn keeps_previous_selection_over_primary() {
        let monitors = [monitor("A", true), monitor("B", false)];
        assert_eq!(preferred_monitor(&monitors, Some("B")), Some("B"));
    }

    #[test]
    fn falls_back_to_primary_when_previous_is_gone() {
        let monitors = [monitor("A", false), monitor("B", true)];
        assert_eq!(preferred_monitor(&monitors, Some("gone")), Some("B"));
    }

    #[test]
    fn falls_back_to_first_without_primary() {
        let monitors = [monitor("A", false), monitor("B", false)];
        assert_eq!(preferred_monitor(&monitors, None), Some("A"));
    }

    #[test]
    fn no_monitors_means_no_preference() {
        assert_eq!(preferred_monitor(&[], Some("A")), None);
    }

    #[test]
    fn subtitle_shows_resolution_position_and_primary() {
        assert_eq!(
            row_subtitle(&monitor("A", true)),
            "2560 × 1440 · Position 0, 142 · Primary"
        );
        assert_eq!(
            row_subtitle(&monitor("A", false)),
            "2560 × 1440 · Position 0, 142"
        );
    }
}
