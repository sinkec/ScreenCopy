//! The short welcome window at startup: what this program is, its version and
//! its license. A click or any key dismisses it early.

use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;

use adw::prelude::*;
use gtk::glib;

use super::{APP_ID, about};

const DISPLAY_TIME: Duration = Duration::from_millis(1800);
const WIDTH: i32 = 440;
const MARGIN: i32 = 28;
const ICON_SIZE: i32 = 128;

fn label(text: &str, classes: &[&str]) -> gtk::Label {
    gtk::Label::builder()
        .label(text)
        .css_classes(classes.to_vec())
        .justify(gtk::Justification::Center)
        .wrap(true)
        .max_width_chars(48)
        .build()
}

/// Shows the splash screen, then runs `finished`, which should open the main
/// window. The splash screen closes afterwards, so the application always has a
/// window and does not quit in between.
pub fn show(app: &adw::Application, finished: impl FnOnce() + 'static) {
    let content = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(10)
        .margin_top(MARGIN)
        .margin_bottom(MARGIN)
        .margin_start(MARGIN)
        .margin_end(MARGIN)
        .build();
    let icon = gtk::Image::builder()
        .icon_name(APP_ID)
        .pixel_size(ICON_SIZE)
        .margin_bottom(6)
        .build();
    content.append(&icon);
    content.append(&label(about::NAME, &["title-1"]));
    content.append(&label(
        &format!("Version {}", about::VERSION),
        &["dim-label"],
    ));
    content.append(&label(about::SUMMARY, &["title-4"]));

    let spinner = gtk::Spinner::builder()
        .spinning(true)
        .margin_top(8)
        .margin_bottom(8)
        .build();
    content.append(&spinner);

    content.append(&label(about::PRIVACY_NOTICE, &["caption"]));
    content.append(&label(about::LICENSE_NOTICE, &["caption", "dim-label"]));
    content.append(&label(about::COPYRIGHT, &["caption", "dim-label"]));

    let window = gtk::Window::builder()
        .application(app)
        .title(about::NAME)
        .icon_name(APP_ID)
        .decorated(false)
        .resizable(false)
        .default_width(WIDTH)
        .child(&content)
        .build();

    let window_for_finish = window.clone();
    let mut finished = Some(finished);
    let finish = Rc::new(RefCell::new(move || {
        if let Some(finished) = finished.take() {
            finished();
            window_for_finish.close();
        }
    }));

    let click = gtk::GestureClick::new();
    let on_click = Rc::clone(&finish);
    click.connect_released(move |_, _, _, _| (on_click.borrow_mut())());
    window.add_controller(click);

    let keys = gtk::EventControllerKey::new();
    let on_key = Rc::clone(&finish);
    keys.connect_key_pressed(move |_, _, _, _| {
        (on_key.borrow_mut())();
        glib::Propagation::Stop
    });
    window.add_controller(keys);

    glib::timeout_add_local_once(DISPLAY_TIME, move || (finish.borrow_mut())());
    window.present();
}
