//! A titled part of the window. All parts share this layout, so that their cards
//! line up and keep the same distances.

use adw::prelude::*;

const HEADER_HEIGHT: i32 = 34;
const HEADER_SPACING: i32 = 12;
const CONTENT_SPACING: i32 = 8;

#[derive(Clone)]
pub struct Section {
    pub root: gtk::Box,
    header: gtk::Box,
    title: gtk::Label,
    detail: gtk::Label,
}

impl Section {
    pub fn new(title: &str) -> Self {
        let title = gtk::Label::builder()
            .label(title)
            .css_classes(["heading"])
            .build();
        // Details can come from the X server, and are never allowed to widen the
        // section.
        let detail = gtk::Label::builder()
            .xalign(0.0)
            .hexpand(true)
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .css_classes(["dim-label"])
            .build();
        let header = gtk::Box::builder()
            .spacing(HEADER_SPACING)
            .height_request(HEADER_HEIGHT)
            .build();
        header.append(&title);
        header.append(&detail);
        let root = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(CONTENT_SPACING)
            .build();
        root.append(&header);
        Self {
            root,
            header,
            title,
            detail,
        }
    }

    pub fn set_title(&self, title: &str) {
        self.title.set_text(title);
    }

    pub fn set_detail(&self, detail: &str) {
        self.detail.set_text(detail);
    }

    pub fn set_suffix(&self, suffix: &impl IsA<gtk::Widget>) {
        suffix.set_valign(gtk::Align::Center);
        self.header.append(suffix);
    }

    pub fn set_content(&self, content: &impl IsA<gtk::Widget>) {
        self.root.append(content);
    }
}
