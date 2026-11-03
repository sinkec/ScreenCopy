//! Test recording of the selected monitor, with a preview and a visible
//! indicator for as long as the screen is being captured.

use std::cell::{Cell, RefCell};
use std::path::Path;
use std::rc::{Rc, Weak};
use std::time::Duration;

use adw::prelude::*;
use gtk::{gdk, gio, glib};
use tracing::{debug, error, info};

use crate::capture::monitor::Monitor;
use crate::capture::recorder::{Encoder, Recorder, RecorderEvent};

const DEFAULT_FILE_NAME: &str = "screencopy-test.ts";
const PREVIEW_INTERVAL: Duration = Duration::from_millis(100);

pub struct Recording {
    window: adw::ApplicationWindow,
    selected: Rc<RefCell<Option<Monitor>>>,
    /// Widgets that must not change while capturing.
    locked: Vec<gtk::Widget>,
    active: Rc<Cell<bool>>,
    banner: adw::Banner,
    record: gtk::Button,
    preview: gtk::Picture,
    show_preview: Box<dyn Fn(bool)>,
    toasts: adw::ToastOverlay,
    recorder: RefCell<Option<Recorder>>,
}

/// The widgets `Recording` needs from the window that hosts it.
pub struct RecordingWidgets {
    pub group: adw::PreferencesGroup,
    pub preview: gtk::Picture,
    /// Called with whether the preview should take the monitors' place.
    pub show_preview: Box<dyn Fn(bool)>,
    pub active: Rc<Cell<bool>>,
    pub window: adw::ApplicationWindow,
    pub banner: adw::Banner,
    pub toasts: adw::ToastOverlay,
    pub locked: Vec<gtk::Widget>,
}

impl Recording {
    /// Fills in the section and connects the controller to the window, which
    /// keeps it alive.
    pub fn attach(widgets: RecordingWidgets, selected: Rc<RefCell<Option<Monitor>>>) {
        let record = gtk::Button::builder()
            .label("Record…")
            .valign(gtk::Align::Center)
            .build();
        let group = widgets.group;
        group.set_title("Test Recording");
        group.set_description(Some(
            "Record the selected monitor to a video file, without a TV.",
        ));
        group.set_header_suffix(Some(&record));

        let this = Rc::new(Self {
            window: widgets.window,
            selected,
            locked: widgets.locked,
            active: widgets.active,
            banner: widgets.banner,
            record,
            preview: widgets.preview,
            show_preview: widgets.show_preview,
            toasts: widgets.toasts,
            recorder: RefCell::default(),
        });
        this.connect_signals();
    }

    fn connect_signals(self: &Rc<Self>) {
        let weak = Rc::downgrade(self);
        self.record.connect_clicked(move |_| {
            if let Some(this) = weak.upgrade() {
                this.choose_file();
            }
        });

        let weak = Rc::downgrade(self);
        self.banner.connect_button_clicked(move |_| {
            if let Some(this) = weak.upgrade() {
                this.request_stop();
            }
        });

        // This strong reference keeps the controller alive with the window.
        // Dropping the recorder stops the capture before the window goes away.
        // The file stays playable because MPEG-TS needs no closing data.
        let this = Rc::clone(self);
        self.window.connect_close_request(move |_| {
            this.recorder.take();
            glib::Propagation::Proceed
        });
    }

    fn choose_file(self: &Rc<Self>) {
        let Some(monitor) = self.selected.borrow().clone() else {
            return;
        };
        let dialog = gtk::FileDialog::builder()
            .title("Save Test Recording")
            .initial_name(DEFAULT_FILE_NAME)
            .modal(true)
            .build();
        if let Some(videos) = glib::user_special_dir(glib::UserDirectory::Videos) {
            dialog.set_initial_folder(Some(&gio::File::for_path(videos)));
        }
        let this = Rc::clone(self);
        dialog.save(
            Some(&self.window),
            gio::Cancellable::NONE,
            move |result| match result {
                Ok(file) => match file.path() {
                    Some(path) => this.start(&monitor, &path),
                    None => this.show_toast("Choose a file on this computer."),
                },
                Err(err) if err.matches(gtk::DialogError::Dismissed) => {
                    debug!("file selection dismissed");
                }
                Err(err) => {
                    error!(error = %err, "file selection failed");
                    this.show_toast("Cannot open the file selection.");
                }
            },
        );
    }

    fn start(self: &Rc<Self>, monitor: &Monitor, path: &Path) {
        let Some(encoder) = Encoder::detect() else {
            error!("no working H.264 encoder");
            self.show_toast("No working H.264 encoder found. Install GStreamer's x264enc.");
            return;
        };
        info!(encoder = encoder.name(), "using encoder");

        let weak = Rc::downgrade(self);
        let saved_as = path.display().to_string();
        let started = Recorder::start(monitor.capture_region(), path, encoder, move |event| {
            if let Some(this) = weak.upgrade() {
                this.finish(event, &saved_as);
            }
        });
        match started {
            Ok(recorder) => {
                self.recorder.replace(Some(recorder));
                self.show_active(monitor);
                schedule_preview(Rc::downgrade(self));
            }
            Err(err) => {
                error!(error = %err, "cannot start recording");
                self.show_toast(&format!("Cannot start recording: {err}"));
            }
        }
    }

    fn request_stop(&self) {
        if let Some(recorder) = self.recorder.borrow().as_ref() {
            self.banner.set_title("Finishing the recording…");
            self.banner.set_button_label(None);
            recorder.stop();
        }
    }

    fn finish(&self, event: RecorderEvent, saved_as: &str) {
        self.recorder.take();
        self.show_idle();
        match event {
            RecorderEvent::Finished => self.show_toast(&format!("Saved {saved_as}")),
            RecorderEvent::Failed(reason) => {
                self.show_toast(&format!("Recording failed: {reason}"));
            }
        }
    }

    fn show_active(&self, monitor: &Monitor) {
        self.active.set(true);
        self.banner
            .set_title(&format!("Recording monitor {}", monitor.name()));
        self.banner.set_button_label(Some("Stop"));
        self.banner.set_revealed(true);
        self.set_controls_enabled(false);
    }

    fn show_idle(&self) {
        self.active.set(false);
        self.banner.set_revealed(false);
        self.preview.set_paintable(gdk::Paintable::NONE);
        (self.show_preview)(false);
        self.set_controls_enabled(true);
    }

    fn set_controls_enabled(&self, enabled: bool) {
        self.record.set_sensitive(enabled);
        for widget in &self.locked {
            widget.set_sensitive(enabled);
        }
    }

    fn show_toast(&self, title: &str) {
        // File names and GStreamer messages are not markup.
        let toast = adw::Toast::builder()
            .title(title)
            .use_markup(false)
            .timeout(8)
            .build();
        self.toasts.add_toast(toast);
    }

    fn update_preview(&self) -> glib::ControlFlow {
        let recorder = self.recorder.borrow();
        let Some(recorder) = recorder.as_ref() else {
            return glib::ControlFlow::Break;
        };
        if let Some(frame) = recorder.pull_preview() {
            let texture = gdk::MemoryTexture::new(
                frame.width,
                frame.height,
                gdk::MemoryFormat::R8g8b8x8,
                &frame.data,
                frame.stride,
            );
            self.preview.set_paintable(Some(&texture));
            (self.show_preview)(true);
        }
        glib::ControlFlow::Continue
    }
}

fn schedule_preview(recording: Weak<Recording>) {
    glib::timeout_add_local(PREVIEW_INTERVAL, move || match recording.upgrade() {
        Some(recording) => recording.update_preview(),
        None => glib::ControlFlow::Break,
    });
}
