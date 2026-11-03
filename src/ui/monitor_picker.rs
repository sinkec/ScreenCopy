//! The monitors, drawn the way they stand on the desk: in their real size,
//! position and rotation, each with a picture of what it shows.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use adw::prelude::*;
use gtk::{gdk, gio, glib};
use tracing::{debug, error};

use crate::capture::monitor::{self, Monitor, MonitorError};
use crate::capture::thumbnail::{self, Thumbnail};

const PAGE_MONITORS: &str = "monitors";
const PAGE_MESSAGE: &str = "message";
const CANVAS_MAX_WIDTH: i32 = 800;
const CANVAS_MAX_HEIGHT: i32 = 180;
const DISPLAY_MONITORS: &str = "monitors";
const DISPLAY_PREVIEW: &str = "preview";
const MIN_TILE_SIDE: i32 = 32;
/// Keeps neighbouring monitors visually apart.
const TILE_GAP: i32 = 4;
const CSS: &str = ".monitor-tile { padding: 0; border-radius: 6px; } \
    .monitor-tile picture { border-radius: 6px; } \
    .monitor-tile:checked { outline: 3px solid @accent_color; outline-offset: 1px; } \
    .monitor-name { margin: 6px; padding: 1px 6px; border-radius: 4px; }";

/// Where one monitor is drawn, in pixels of the picker.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Tile {
    x: i32,
    y: i32,
    width: i32,
    height: i32,
}

/// Scales the monitors' positions and sizes, given as `(x, y, width, height)`
/// on the X screen, to fit the picker. RandR already reports a rotated monitor
/// with its rotated size, so a portrait monitor comes out upright.
fn layout_tiles(monitors: &[(u32, u32, u32, u32)], max_width: i32, max_height: i32) -> Vec<Tile> {
    let min_x = monitors.iter().map(|m| m.0).min().unwrap_or(0);
    let min_y = monitors.iter().map(|m| m.1).min().unwrap_or(0);
    let right = monitors.iter().map(|m| m.0 + m.2).max().unwrap_or(1);
    let bottom = monitors.iter().map(|m| m.1 + m.3).max().unwrap_or(1);
    let scale = f64::min(
        f64::from(max_width) / f64::from((right - min_x).max(1)),
        f64::from(max_height) / f64::from((bottom - min_y).max(1)),
    );
    // Everything is bounded by the picker's size, so the casts are lossless.
    let scaled = |value: u32| (f64::from(value) * scale).round() as i32;
    monitors
        .iter()
        .map(|&(x, y, width, height)| Tile {
            x: scaled(x - min_x),
            y: scaled(y - min_y),
            width: (scaled(width) - TILE_GAP).max(MIN_TILE_SIDE),
            height: (scaled(height) - TILE_GAP).max(MIN_TILE_SIDE),
        })
        .collect()
}

pub struct MonitorPicker {
    pub stack: gtk::Stack,
    /// The section with the monitors, for the top of the window.
    pub section: adw::PreferencesGroup,
    /// What must not change while capturing.
    pub canvas: gtk::Fixed,
    /// Shows a recording's preview in the monitors' place, so that the window
    /// keeps its size.
    pub preview: gtk::Picture,
    display: gtk::Stack,
    group: adw::PreferencesGroup,
    message: adw::StatusPage,
    pub selected: Rc<RefCell<Option<Monitor>>>,
    /// Lets an older round of pictures notice that it was replaced.
    generation: Cell<u64>,
}

impl MonitorPicker {
    /// `content` is what the window shows, with `section` somewhere inside.
    pub fn new(content: &impl IsA<gtk::Widget>) -> Rc<Self> {
        let canvas = gtk::Fixed::builder()
            .halign(gtk::Align::Center)
            .margin_top(6)
            .margin_bottom(6)
            .build();
        let preview = gtk::Picture::builder()
            .content_fit(gtk::ContentFit::Contain)
            .can_shrink(true)
            .build();
        let display = gtk::Stack::builder()
            .height_request(CANVAS_MAX_HEIGHT + 12)
            .build();
        display.add_named(&canvas, Some(DISPLAY_MONITORS));
        display.add_named(&preview, Some(DISPLAY_PREVIEW));

        let group = adw::PreferencesGroup::builder().title("Monitor").build();
        group.add(&display);

        let message = adw::StatusPage::builder()
            .icon_name("video-display-symbolic")
            .build();

        let stack = gtk::Stack::new();
        stack.add_named(content, Some(PAGE_MONITORS));
        stack.add_named(&message, Some(PAGE_MESSAGE));

        Rc::new(Self {
            stack,
            section: group.clone(),
            canvas,
            preview,
            display,
            group,
            message,
            selected: Rc::default(),
            generation: Cell::new(0),
        })
    }

    /// Shows the recording's preview, or the monitors again.
    pub fn show_preview(&self, showing: bool) {
        self.display.set_visible_child_name(if showing {
            DISPLAY_PREVIEW
        } else {
            DISPLAY_MONITORS
        });
    }

    /// Reads the monitors and takes new pictures, keeping the selection if the
    /// monitor still exists. This is the only time the screen is captured
    /// outside of sharing and recording.
    pub fn refresh(self: &Rc<Self>) {
        self.show(monitor::list_monitors());
    }

    fn show_message(&self, title: &str, description: Option<&str>) {
        self.selected.replace(None);
        self.message.set_title(title);
        self.message.set_description(description);
        self.stack.set_visible_child_name(PAGE_MESSAGE);
    }

    fn show(self: &Rc<Self>, result: Result<Vec<Monitor>, MonitorError>) {
        while let Some(tile) = self.canvas.first_child() {
            self.canvas.remove(&tile);
        }
        let monitors = match result {
            Ok(monitors) if monitors.is_empty() => {
                self.show_message("No Monitors Found", None);
                return;
            }
            Ok(monitors) => monitors,
            Err(err) => {
                error!(error = %err, "cannot read monitors");
                // The status page renders its description as markup.
                let description = glib::markup_escape_text(&err.to_string());
                self.show_message("Cannot Read Monitors", Some(&description));
                return;
            }
        };

        let geometry: Vec<_> = monitors
            .iter()
            .map(|monitor| {
                let (x, y) = monitor.position();
                let (width, height) = monitor.resolution();
                (x, y, width, height)
            })
            .collect();
        let tiles = layout_tiles(&geometry, CANVAS_MAX_WIDTH, CANVAS_MAX_HEIGHT);

        let previous = self.selected.borrow().clone();
        let preferred =
            preferred_monitor(&monitors, previous.as_ref().map(Monitor::name)).map(str::to_owned);
        let mut button_group: Option<gtk::ToggleButton> = None;
        let mut pictures = Vec::with_capacity(monitors.len());
        for (monitor, tile) in monitors.into_iter().zip(tiles) {
            let is_preferred = preferred.as_deref() == Some(monitor.name());
            let region = monitor.capture_region();
            let (button, picture) = self.add_tile(monitor, tile);
            button.set_group(button_group.as_ref());
            if is_preferred {
                button.set_active(true);
            }
            button_group.get_or_insert(button);
            pictures.push((picture, region, tile));
        }
        self.stack.set_visible_child_name(PAGE_MONITORS);
        self.load_pictures(pictures);
    }

    fn add_tile(&self, monitor: Monitor, tile: Tile) -> (gtk::ToggleButton, gtk::Picture) {
        let picture = gtk::Picture::builder()
            .content_fit(gtk::ContentFit::Fill)
            .can_shrink(true)
            .build();
        // Monitor names come from the X server, so they are not markup.
        let name = gtk::Label::builder()
            .label(monitor.name())
            .use_markup(false)
            .halign(gtk::Align::Start)
            .valign(gtk::Align::End)
            .css_classes(["osd", "caption", "monitor-name"])
            .build();
        let overlay = gtk::Overlay::builder().child(&picture).build();
        overlay.add_overlay(&name);

        let button = gtk::ToggleButton::builder()
            .child(&overlay)
            .width_request(tile.width)
            .height_request(tile.height)
            .tooltip_text(details(&monitor))
            .css_classes(["monitor-tile"])
            .build();
        self.canvas
            .put(&button, f64::from(tile.x), f64::from(tile.y));

        let selected = Rc::clone(&self.selected);
        let group = self.group.clone();
        button.connect_toggled(move |button| {
            if button.is_active() {
                debug!(
                    monitor = monitor.name(),
                    region = %monitor.capture_region(),
                    "monitor selected"
                );
                group.set_description(Some(&glib::markup_escape_text(&details(&monitor))));
                selected.replace(Some(monitor.clone()));
            }
        });
        (button, picture)
    }

    /// Takes the pictures on another thread, because each takes a moment.
    fn load_pictures(
        self: &Rc<Self>,
        pictures: Vec<(gtk::Picture, crate::capture::monitor::CaptureRegion, Tile)>,
    ) {
        let generation = self.generation.get() + 1;
        self.generation.set(generation);
        let this = Rc::clone(self);
        glib::spawn_future_local(async move {
            for (picture, region, tile) in pictures {
                let taken = gio::spawn_blocking(move || {
                    thumbnail::capture(region, tile.width, tile.height)
                })
                .await;
                if this.generation.get() != generation {
                    return;
                }
                match taken {
                    Ok(Some(thumbnail)) => picture.set_paintable(Some(&texture(thumbnail))),
                    _ => debug!("no picture of this monitor"),
                }
            }
        });
    }
}

fn texture(thumbnail: Thumbnail) -> gdk::MemoryTexture {
    gdk::MemoryTexture::new(
        thumbnail.width,
        thumbnail.height,
        // The picture has no alpha channel; see `thumbnail`.
        gdk::MemoryFormat::R8g8b8x8,
        &glib::Bytes::from_owned(thumbnail.pixels),
        thumbnail.stride,
    )
}

/// Adds the styles the monitor tiles need.
pub fn load_style() {
    if let Some(display) = gdk::Display::default() {
        let provider = gtk::CssProvider::new();
        provider.load_from_string(CSS);
        gtk::style_context_add_provider_for_display(
            &display,
            &provider,
            gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
        );
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

fn details(monitor: &Monitor) -> String {
    let (width, height) = monitor.resolution();
    let (x, y) = monitor.position();
    let primary = if monitor.is_primary() {
        " · Primary"
    } else {
        ""
    };
    format!(
        "{}: {width} × {height} · Position {x}, {y}{primary}",
        monitor.name()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn monitor(name: &str, primary: bool) -> Monitor {
        Monitor::new(name.to_owned(), 0, 142, 2560, 1440, primary).unwrap()
    }

    /// The test computer: landscape, portrait and landscape side by side.
    const DESK: [(u32, u32, u32, u32); 3] = [
        (0, 142, 2560, 1440),
        (2560, 0, 1080, 1920),
        (3640, 840, 1920, 1080),
    ];

    #[test]
    fn a_portrait_monitor_is_drawn_upright() {
        let tiles = layout_tiles(&DESK, 620, 230);
        assert!(tiles[1].height > tiles[1].width, "{tiles:?}");
        assert!(tiles[0].width > tiles[0].height, "{tiles:?}");
    }

    #[test]
    fn monitors_keep_their_relative_sizes() {
        let tiles = layout_tiles(&DESK, 620, 230);
        assert!(tiles[0].width > tiles[2].width, "{tiles:?}");
        assert!(tiles[2].width > tiles[1].width, "{tiles:?}");
    }

    #[test]
    fn monitors_keep_their_positions_without_overlapping() {
        let tiles = layout_tiles(&DESK, 620, 230);
        assert!(tiles[0].x + tiles[0].width <= tiles[1].x, "{tiles:?}");
        assert!(tiles[1].x + tiles[1].width <= tiles[2].x, "{tiles:?}");
        assert_eq!(tiles[1].y, 0, "the tallest starts at the top: {tiles:?}");
        assert!(tiles[2].y > tiles[0].y, "{tiles:?}");
    }

    #[test]
    fn the_whole_desk_fits_the_picker() {
        for (width, height) in [(620, 230), (200, 500), (620, 40)] {
            let tiles = layout_tiles(&DESK[..], width, height);
            for tile in tiles {
                assert!(tile.y + tile.height <= height.max(MIN_TILE_SIDE + tile.y));
                assert!(tile.x + tile.width <= width + MIN_TILE_SIDE);
            }
        }
    }

    #[test]
    fn a_monitor_stacked_above_another_stays_above() {
        let tiles = layout_tiles(&[(0, 0, 1920, 1080), (0, 1080, 1920, 1080)], 620, 230);
        assert_eq!(tiles[0].x, tiles[1].x);
        assert!(tiles[0].y + tiles[0].height <= tiles[1].y, "{tiles:?}");
    }

    #[test]
    fn offsets_from_the_origin_are_removed() {
        let tiles = layout_tiles(&[(1000, 500, 1920, 1080)], 620, 230);
        assert_eq!((tiles[0].x, tiles[0].y), (0, 0));
    }

    #[test]
    fn no_monitors_mean_no_tiles() {
        assert!(layout_tiles(&[], 620, 230).is_empty());
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
    fn details_show_name_resolution_position_and_primary() {
        assert_eq!(
            details(&monitor("A", true)),
            "A: 2560 × 1440 · Position 0, 142 · Primary"
        );
        assert_eq!(
            details(&monitor("A", false)),
            "A: 2560 × 1440 · Position 0, 142"
        );
    }
}
