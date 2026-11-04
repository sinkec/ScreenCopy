//! The main window, and what happens when it closes.

use std::cell::Cell;
use std::rc::Rc;
use std::sync::mpsc::{Receiver, TryRecvError};
use std::time::Duration;

use adw::prelude::*;
use gtk::{gio, glib};
use tracing::{error, info};

use super::monitor_picker::{self, MonitorPicker};
use super::recording::{Recording, RecordingWidgets};
use super::section::Section;
use super::tray::{Tray, TrayCommand};
use super::tv::{ShareSource, TvPanel, TvWidgets};
use super::video_file::VideoFilePanel;
use super::{APP_ID, about};
use crate::audio;
use crate::capture::file_stream::FilmQuality;
use crate::firewall;
use crate::settings::{SettingsStore, Theme};

const TRAY_POLL_INTERVAL: Duration = Duration::from_millis(200);
/// Everything fits without scrolling, also on a 1366 × 768 screen.
const WINDOW_WIDTH: i32 = 970;
const WINDOW_HEIGHT: i32 = 630;
/// The one distance around and between the parts of the window.
const SPACING: i32 = 18;
const FILM_QUALITIES: [(&str, FilmQuality); 3] = [
    ("Standard", FilmQuality::Standard),
    ("High", FilmQuality::High),
    ("Highest", FilmQuality::Highest),
];
const THEMES: [(&str, Theme); 3] = [
    ("System", Theme::System),
    ("Light", Theme::Light),
    ("Dark", Theme::Dark),
];

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
    monitor_picker::load_style();
    #[cfg(debug_assertions)]
    if let Some(display) = gtk::gdk::Display::default() {
        gtk::IconTheme::for_display(&display)
            .add_search_path(concat!(env!("CARGO_MANIFEST_DIR"), "/data/icons"));
    }
}

fn apply_theme(theme: Theme) {
    adw::StyleManager::default().set_color_scheme(match theme {
        Theme::System => adw::ColorScheme::Default,
        Theme::Light => adw::ColorScheme::ForceLight,
        Theme::Dark => adw::ColorScheme::ForceDark,
    });
}

/// Applies the saved theme, before the first window appears.
pub fn prepare_theme() {
    apply_theme(SettingsStore::load().get().theme);
}

fn application_group(settings: &SettingsStore, has_tray: bool) -> adw::PreferencesGroup {
    let names: Vec<&str> = THEMES.iter().map(|(name, _)| *name).collect();
    let theme = adw::ComboRow::builder()
        .title("Appearance")
        .model(&gtk::StringList::new(&names))
        .build();
    let current = THEMES
        .iter()
        .position(|(_, theme)| *theme == settings.get().theme);
    // The list is short enough for the conversion to be lossless.
    theme.set_selected(current.unwrap_or(0) as u32);
    let store = settings.clone();
    theme.connect_selected_notify(move |row| {
        let chosen = THEMES.get(row.selected() as usize).map(|(_, theme)| *theme);
        let theme = chosen.unwrap_or_default();
        apply_theme(theme);
        store.update(|settings| settings.theme = theme);
    });

    let quality_names: Vec<&str> = FILM_QUALITIES.iter().map(|(name, _)| *name).collect();
    let quality = adw::ComboRow::builder()
        .title("Video File Quality")
        .subtitle("Choose a lower quality if a film stutters on the TV.")
        .model(&gtk::StringList::new(&quality_names))
        .build();
    let current_quality = FILM_QUALITIES
        .iter()
        .position(|(_, quality)| *quality == settings.get().film_quality);
    // The list is short enough for the conversion to be lossless.
    quality.set_selected(current_quality.unwrap_or(1) as u32);
    let store = settings.clone();
    quality.connect_selected_notify(move |row| {
        let quality = FILM_QUALITIES
            .get(row.selected() as usize)
            .map(|(_, quality)| *quality)
            .unwrap_or_default();
        store.update(|settings| settings.film_quality = quality);
    });

    let group = adw::PreferencesGroup::builder()
        .title("Application")
        .build();
    group.add(&theme);
    group.add(&quality);
    if has_tray {
        let keep = adw::SwitchRow::builder()
            .title("Keep Running in the Tray")
            .subtitle("Closing the window leaves MonitorBeam in the system tray.")
            .active(settings.get().close_to_tray)
            .build();
        let store = settings.clone();
        keep.connect_active_notify(move |row| {
            let active = row.is_active();
            store.update(|settings| settings.close_to_tray = active);
        });
        group.add(&keep);
    }
    group
}

/// Says once where the program went, because it is otherwise not obvious.
fn show_tray_notice(app: &adw::Application, settings: &SettingsStore) {
    if settings.get().tray_notice_shown {
        return;
    }
    settings.update(|settings| settings.tray_notice_shown = true);
    let notification = gio::Notification::new("MonitorBeam is still running");
    notification.set_body(Some(
        "Open it from the system tray, or right-click the icon there to quit.",
    ));
    app.send_notification(Some("tray"), &notification);
}

/// Carries out what the tray asks for, on the GTK thread.
fn listen_to_tray(
    app: &adw::Application,
    window: &adw::ApplicationWindow,
    commands: Receiver<TrayCommand>,
) {
    let app = app.clone();
    let window = window.clone();
    glib::timeout_add_local(TRAY_POLL_INTERVAL, move || {
        loop {
            match commands.try_recv() {
                Ok(TrayCommand::Show) => window.present(),
                Ok(TrayCommand::StopSharing) => {
                    WidgetExt::activate_action(&window, "win.stop-sharing", None).ok();
                }
                Ok(TrayCommand::PlayOrPause) => {
                    WidgetExt::activate_action(&window, "win.play-or-pause", None).ok();
                }
                Ok(TrayCommand::Quit) => app.activate_action("quit", None),
                Err(TryRecvError::Empty) => return glib::ControlFlow::Continue,
                Err(TryRecvError::Disconnected) => return glib::ControlFlow::Break,
            }
        }
    });
}

/// The settings that are not needed while sharing, in a window of their own.
fn show_preferences(window: &adw::ApplicationWindow, settings: &SettingsStore, has_tray: bool) {
    let page = adw::PreferencesPage::new();
    page.add(&application_group(settings, has_tray));
    let dialog = adw::PreferencesDialog::new();
    dialog.add(&page);
    dialog.present(Some(window));
}

pub fn build(app: &adw::Application) -> adw::ApplicationWindow {
    let settings = SettingsStore::load();
    let layout = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(SPACING)
        .margin_top(SPACING)
        .margin_bottom(SPACING)
        .margin_start(SPACING)
        .margin_end(SPACING)
        .build();
    let picker = MonitorPicker::new(&layout);
    picker.refresh();

    let refresh = gtk::Button::builder()
        .icon_name("view-refresh-symbolic")
        .tooltip_text("Refresh Monitors and Pictures")
        .build();

    picker.section.set_suffix(&refresh);

    let main_items = gio::Menu::new();
    main_items.append(Some("Test Recording…"), Some("win.record"));
    main_items.append(Some("Preferences"), Some("win.preferences"));
    main_items.append(Some("About MonitorBeam"), Some("win.about"));
    let quit_item = gio::Menu::new();
    quit_item.append(Some("Quit"), Some("app.quit"));
    let menu = gio::Menu::new();
    menu.append_section(None, &main_items);
    menu.append_section(None, &quit_item);
    let menu_button = gtk::MenuButton::builder()
        .icon_name("open-menu-symbolic")
        .tooltip_text("Main Menu")
        .menu_model(&menu)
        .build();
    app.set_accels_for_action("app.quit", &["<Control>q"]);
    app.set_accels_for_action("win.preferences", &["<Control>comma"]);
    app.set_accels_for_action("window.close", &["<Control>w"]);

    // Says when the screen is being captured, wherever the rest of the window is.
    let title = adw::WindowTitle::new(about::NAME, "");
    let header = adw::HeaderBar::new();
    header.set_title_widget(Some(&title));
    header.pack_end(&menu_button);

    let content = adw::ToolbarView::new();
    content.add_top_bar(&header);
    content.set_content(Some(&picker.stack));

    let toasts = adw::ToastOverlay::new();
    toasts.set_child(Some(&content));
    restore_firewall(&toasts);
    restore_audio(&toasts);

    // A non-resizable window follows what its content asks for, which changes with
    // every text. This container asks for a fixed size instead, and shows no
    // scrollbars. The toasts belong inside it, because they are measured too.
    let fixed_size = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::External)
        .vscrollbar_policy(gtk::PolicyType::External)
        .min_content_width(WINDOW_WIDTH)
        .min_content_height(WINDOW_HEIGHT)
        .child(&toasts)
        .build();

    let window = adw::ApplicationWindow::builder()
        .application(app)
        .title(about::NAME)
        .icon_name(APP_ID)
        .resizable(false)
        .content(&fixed_size)
        .build();

    let about_action = gio::ActionEntry::builder("about")
        .activate(|window: &adw::ApplicationWindow, _, _| about::show_dialog(window))
        .build();
    window.add_action_entries([about_action]);

    let tray = Tray::spawn();
    let has_tray = tray.is_some();
    let (tray, commands) = tray.map_or((None, None), |(tray, commands)| {
        (Some(Rc::new(tray)), Some(commands))
    });
    if let Some(commands) = commands {
        listen_to_tray(app, &window, commands);
    }

    // This must come first. When the window only hides, the handlers connected
    // below, which end whatever is running, must not run.
    let quitting = Rc::new(Cell::new(false));
    let recording_active = Rc::new(Cell::new(false));
    {
        let (app, settings, quitting, recording_active) = (
            app.clone(),
            settings.clone(),
            Rc::clone(&quitting),
            Rc::clone(&recording_active),
        );
        window.connect_close_request(move |window| {
            // A test recording has no controls in the tray, so it does not
            // continue without its window.
            let leave_running = has_tray
                && settings.get().close_to_tray
                && !quitting.get()
                && !recording_active.get();
            if !leave_running {
                return glib::Propagation::Proceed;
            }
            info!("the window closes, the program stays in the tray");
            window.set_visible(false);
            show_tray_notice(&app, &settings);
            glib::Propagation::Stop
        });
    }
    {
        let (quitting, window) = (Rc::clone(&quitting), window.clone());
        let quit = gio::ActionEntry::builder("quit")
            .activate(move |app: &adw::Application, _, _| {
                quitting.set(true);
                if window.is_realized() {
                    // Ends a running session properly, then the program.
                    window.close();
                } else {
                    app.quit();
                }
            })
            .build();
        app.add_action_entries([quit]);
    }

    // Sharing and recording exclude each other, and neither allows changing
    // the monitor while running.
    let tv_section = Section::new("TV");
    let picture_section = Section::new("Picture and Sound");
    let record_action = gio::SimpleAction::new("record", None);
    window.add_action(&record_action);
    let monitor_controls: [gtk::Widget; 4] = [
        picker.canvas.clone().upcast(),
        refresh.clone().upcast(),
        picker.monitor_toggle.clone().upcast(),
        picker.file_toggle.clone().upcast(),
    ];

    let video_file = VideoFilePanel::new(&window, &toasts, &settings);
    picker.attach_file_page(&video_file.root, &refresh);
    if let Some(tray) = &tray {
        let tray = Rc::clone(tray);
        video_file.connect_playing(move |playing| tray.set_film_playing(playing));
    }

    TvPanel::attach(TvWidgets {
        section: tv_section.clone(),
        picture_section: picture_section.clone(),
        settings: settings.clone(),
        on_sharing: {
            let record_action = record_action.clone();
            let video_file = Rc::clone(&video_file);
            Box::new(move |sharing| {
                record_action.set_enabled(!sharing);
                if !sharing {
                    video_file.session_ended();
                }
                if let Some(tray) = &tray {
                    tray.set_sharing(sharing);
                }
            })
        },
        window: window.clone(),
        title: title.clone(),
        toasts: toasts.clone(),
        locked: monitor_controls.to_vec(),
        source: {
            let picker = Rc::clone(&picker);
            let video_file = Rc::clone(&video_file);
            let settings = settings.clone();
            Box::new(move || {
                if picker.file_is_source() {
                    let file = video_file
                        .prepare_to_share()
                        .ok_or("Add a video file first.".to_owned())?;
                    return Ok(ShareSource::File(
                        file.playback,
                        settings.get().film_quality,
                    ));
                }
                let monitor = picker.selected.borrow().clone();
                monitor
                    .map(ShareSource::Monitor)
                    .ok_or("Choose a monitor first.".to_owned())
            })
        },
        file_toggle: picker.file_toggle.clone(),
    });

    let mut locked_by_recording = monitor_controls.to_vec();
    locked_by_recording.push(tv_section.root.clone().upcast());
    locked_by_recording.push(picture_section.root.clone().upcast());
    locked_by_recording.push(video_file.root.clone().upcast());
    Recording::attach(
        RecordingWidgets {
            action: record_action,
            status: picker.recording_status.clone(),
            stop: picker.recording_stop.clone(),
            preview: picker.preview.clone(),
            show_preview: {
                let picker = Rc::clone(&picker);
                Box::new(move |showing| picker.show_preview(showing))
            },
            active: recording_active,
            window: window.clone(),
            title,
            toasts,
            locked: locked_by_recording,
        },
        Rc::clone(&picker.selected),
    );
    let columns = gtk::Box::builder()
        .spacing(SPACING)
        .homogeneous(true)
        .build();
    columns.append(&tv_section.root);
    columns.append(&picture_section.root);
    layout.append(&picker.section.root);
    layout.append(&columns);

    {
        let settings = settings.clone();
        let preferences = gio::ActionEntry::builder("preferences")
            .activate(move |window: &adw::ApplicationWindow, _, _| {
                show_preferences(window, &settings, has_tray);
            })
            .build();
        window.add_action_entries([preferences]);
    }

    let picker_for_refresh = Rc::clone(&picker);
    refresh.connect_clicked(move |_| picker_for_refresh.refresh());

    window
}
