//! ScreenCopy clones one selected monitor to a TV over Miracast.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use adw::prelude::*;
use gtk::glib;
use screencopy::ui;
use signal_hook::consts::{SIGINT, SIGTERM};
use tracing::error;
use tracing_subscriber::EnvFilter;

const SIGNAL_POLL_INTERVAL: Duration = Duration::from_millis(200);

fn main() -> glib::ExitCode {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .init();

    if let Err(err) = gst::init() {
        error!(error = %err, "cannot initialize GStreamer");
        return glib::ExitCode::FAILURE;
    }

    let app = adw::Application::builder()
        .application_id(ui::APP_ID)
        .build();
    app.connect_startup(|_| ui::window::prepare_icons());
    app.connect_activate(|app| match app.active_window() {
        Some(window) => window.present(),
        None => {
            let app_for_window = app.clone();
            ui::splash::show(app, move || ui::window::build(&app_for_window).present());
        }
    });
    if let Err(err) = close_window_on_signals(&app) {
        error!(error = %err, "cannot watch for termination signals");
    }
    app.run()
}

/// Closing the window ends a running session properly, which puts the sound
/// settings back. Without this, Ctrl+C or logging out would leave that to the
/// next start of the program.
fn close_window_on_signals(app: &adw::Application) -> std::io::Result<()> {
    let requested = Arc::new(AtomicBool::new(false));
    for signal in [SIGINT, SIGTERM] {
        signal_hook::flag::register(signal, Arc::clone(&requested))?;
    }
    let app = app.clone();
    glib::timeout_add_local(SIGNAL_POLL_INTERVAL, move || {
        if requested.swap(false, Ordering::Relaxed) {
            match app.active_window() {
                Some(window) => window.close(),
                None => app.quit(),
            }
        }
        glib::ControlFlow::Continue
    });
    Ok(())
}
