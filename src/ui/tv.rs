//! Finding Miracast receivers and sharing the selected monitor with one.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::sync::mpsc::{self, TryRecvError};
use std::time::Duration;

use adw::prelude::*;
use gtk::{gio, glib};
use tracing::{debug, error};

use crate::capture::monitor::Monitor;
use crate::capture::recorder::Encoder;
use crate::firewall;
use crate::p2p::{P2pClient, P2pError, P2pLink, Peer, SEARCH_SECONDS};
use crate::settings::{
    FRAME_RATES, MAX_SOUND_DELAY_MS, MIN_SOUND_DELAY_MS, SettingsStore, VIDEO_HEIGHTS,
};
use crate::wfd::params::VideoPreference;
use crate::wfd::session::{SessionEvent, Sound};
use crate::wfd::{self, ServeConfig, ServeEvent};

const REFRESH_INTERVAL: Duration = Duration::from_secs(1);
/// Room for this many TVs without scrolling.
const TVS_WITHOUT_SCROLLING: usize = 2;
const TV_LIST_HEIGHT: i32 = 120;
/// Two lines of text. The longest status needs them, and the shorter ones must
/// not make the content below move.
const STATUS_HEIGHT: i32 = 40;
const STATUS_LINES: i32 = 2;
const SOUND_DELAY_STEP_MS: f64 = 10.0;
const EVENT_POLL_INTERVAL: Duration = Duration::from_millis(100);
const SEARCH_DESCRIPTION: &str = "Turn on Screen Mirroring on the TV, then search for it.";

/// The widgets `TvPanel` needs from the window that hosts it.
pub struct TvWidgets {
    pub group: adw::PreferencesGroup,
    pub picture_group: adw::PreferencesGroup,
    pub settings: SettingsStore,
    /// Called when sharing starts and ends.
    pub on_sharing: Box<dyn Fn(bool)>,
    pub window: adw::ApplicationWindow,
    pub banner: adw::Banner,
    pub toasts: adw::ToastOverlay,
    /// Widgets that must not change while sharing.
    pub locked: Vec<gtk::Widget>,
}

pub struct TvPanel {
    widgets: TvWidgets,
    selected: Rc<RefCell<Option<Monitor>>>,
    list: gtk::ListBox,
    status: gtk::Label,
    search: gtk::Button,
    resolution: adw::ComboRow,
    frame_rate: adw::ComboRow,
    client: RefCell<Option<Rc<P2pClient>>>,
    shown_peers: RefCell<Vec<Peer>>,
    /// Lets a running search notice that it was replaced or cancelled.
    search_generation: Cell<u64>,
    stop: RefCell<Option<Arc<AtomicBool>>>,
    sound_delay_ms: Arc<AtomicI32>,
    /// The window closes once the running session has cleaned up.
    close_requested: Cell<bool>,
}

impl TvPanel {
    /// Fills in the section. The buttons' signal handlers keep the panel alive
    /// with the window.
    pub fn attach(widgets: TvWidgets, selected: Rc<RefCell<Option<Monitor>>>) {
        let search = gtk::Button::builder()
            .label("Search")
            .valign(gtk::Align::Center)
            .build();
        let list = gtk::ListBox::builder()
            .selection_mode(gtk::SelectionMode::None)
            .css_classes(["boxed-list"])
            .visible(false)
            .build();
        widgets.group.set_title("TV");
        widgets.group.set_header_suffix(Some(&search));
        let resolution = choice_row(
            "Resolution",
            "Recommended",
            &VIDEO_HEIGHTS,
            |height| format!("{height}p"),
            widgets.settings.get().video_height,
        );
        let frame_rate = choice_row(
            "Frame Rate",
            "Recommended (30 fps)",
            &FRAME_RATES,
            |rate| {
                if rate == 60 {
                    "60 fps (can stutter more)".to_owned()
                } else {
                    format!("{rate} fps")
                }
            },
            widgets.settings.get().frame_rate,
        );
        let settings = widgets.settings.clone();
        resolution.connect_selected_notify(move |row| {
            let choice = chosen(row, &VIDEO_HEIGHTS);
            settings.update(|settings| settings.video_height = choice);
        });
        let settings = widgets.settings.clone();
        frame_rate.connect_selected_notify(move |row| {
            let choice = chosen(row, &FRAME_RATES);
            settings.update(|settings| settings.frame_rate = choice);
        });
        let sound_delay_ms = Arc::new(AtomicI32::new(widgets.settings.get().sound_delay_ms));
        widgets.picture_group.set_title("Picture and Sound");
        widgets.picture_group.add(&resolution);
        widgets.picture_group.add(&frame_rate);
        widgets.picture_group.add(&sound_delay_row(
            widgets.settings.clone(),
            Arc::clone(&sound_delay_ms),
        ));
        // TV names come over the air, so the status is not markup. A very small
        // natural width keeps a long text from widening the column.
        let status = gtk::Label::builder()
            .label(SEARCH_DESCRIPTION)
            .use_markup(false)
            .xalign(0.0)
            .valign(gtk::Align::Start)
            .wrap(true)
            .lines(STATUS_LINES)
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .max_width_chars(1)
            .height_request(STATUS_HEIGHT)
            .css_classes(["dim-label"])
            .build();
        // Room for two TVs. Any more scroll within this space, so that the window
        // keeps its size.
        let list_space = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            // Always visible when needed, or further TVs could go unnoticed.
            .overlay_scrolling(false)
            .min_content_height(TV_LIST_HEIGHT)
            .max_content_height(TV_LIST_HEIGHT)
            .child(&list)
            .build();
        let tv_area = gtk::Box::new(gtk::Orientation::Vertical, 0);
        tv_area.append(&status);
        tv_area.append(&list_space);
        widgets.group.add(&tv_area);

        let this = Rc::new(Self {
            widgets,
            selected,
            list,
            status,
            search,
            resolution,
            frame_rate,
            client: RefCell::default(),
            shown_peers: RefCell::default(),
            search_generation: Cell::new(0),
            stop: RefCell::default(),
            sound_delay_ms,
            close_requested: Cell::new(false),
        });

        let panel = Rc::clone(&this);
        this.search.connect_clicked(move |_| {
            let panel = Rc::clone(&panel);
            glib::spawn_future_local(async move { panel.run_search().await });
        });
        let panel = Rc::clone(&this);
        this.widgets
            .banner
            .connect_button_clicked(move |_| panel.request_stop());
        let panel = Rc::clone(&this);
        let stop_action = gio::ActionEntry::builder("stop-sharing")
            .activate(move |_: &adw::ApplicationWindow, _, _| panel.request_stop())
            .build();
        this.widgets.window.add_action_entries([stop_action]);
        // A session must end properly before the program exits: the sound
        // settings are put back by its thread.
        let panel = Rc::clone(&this);
        this.widgets.window.connect_close_request(move |_| {
            let Some(stop) = panel.stop.borrow().clone() else {
                return glib::Propagation::Proceed;
            };
            panel.close_requested.set(true);
            panel.widgets.banner.set_title("Stopping…");
            panel.widgets.banner.set_button_label(None);
            stop.store(true, Ordering::Relaxed);
            glib::Propagation::Stop
        });
    }

    /// The NetworkManager client, created on first use.
    async fn client(&self) -> Result<Rc<P2pClient>, P2pError> {
        let existing = self.client.borrow().clone();
        if let Some(client) = existing {
            return Ok(client);
        }
        let client = Rc::new(P2pClient::new().await?);
        self.client.replace(Some(Rc::clone(&client)));
        Ok(client)
    }

    fn describe(&self, text: &str) {
        self.status.set_text(text);
    }

    async fn run_search(self: Rc<Self>) {
        let generation = self.search_generation.get() + 1;
        self.search_generation.set(generation);
        self.search.set_sensitive(false);
        self.describe("Searching…");
        self.shown_peers.borrow_mut().clear();
        self.list.remove_all();
        self.list.set_visible(false);

        match self.search_until_finished(generation).await {
            Ok(()) => {
                // A newer search or a session has taken over the panel.
                if self.search_generation.get() == generation {
                    self.search.set_sensitive(true);
                    self.describe(&search_result(self.shown_peers.borrow().len()));
                }
            }
            Err(err) => {
                error!(error = %err, "search failed");
                self.search.set_sensitive(true);
                self.describe(SEARCH_DESCRIPTION);
                self.show_toast(&err.to_string());
            }
        }
    }

    async fn search_until_finished(self: &Rc<Self>, generation: u64) -> Result<(), P2pError> {
        let client = self.client().await?;
        client.start_search().await?;
        for _ in 0..SEARCH_SECONDS {
            glib::timeout_future(REFRESH_INTERVAL).await;
            if self.search_generation.get() != generation {
                return Ok(());
            }
            self.show_peers(client.peers().await?);
        }
        Ok(())
    }

    /// Rebuilds the list only if it changed, so that it does not move under
    /// the pointer. Signal strength alone does not count as a change.
    fn show_peers(self: &Rc<Self>, peers: Vec<Peer>) {
        let same_devices = {
            let shown = self.shown_peers.borrow();
            shown.len() == peers.len()
                && shown
                    .iter()
                    .zip(&peers)
                    .all(|(a, b)| a.hw_address == b.hw_address && a.name == b.name)
        };
        if same_devices {
            return;
        }
        self.list.remove_all();
        for peer in &peers {
            self.add_row(peer.clone());
        }
        self.list.set_visible(!peers.is_empty());
        self.shown_peers.replace(peers);
    }

    fn add_row(self: &Rc<Self>, peer: Peer) {
        let share = gtk::Button::builder()
            .label("Share")
            .valign(gtk::Align::Center)
            .css_classes(["suggested-action"])
            .sensitive(peer.info.available)
            .build();
        // Names come over the air, so they are not markup.
        let row = adw::ActionRow::builder()
            .use_markup(false)
            .title(&peer.name)
            .title_lines(1)
            .tooltip_text(&peer.name)
            .subtitle(if peer.info.available { "Ready" } else { "Busy" })
            .build();
        row.add_suffix(&share);
        self.list.append(&row);

        let panel = Rc::clone(self);
        share.connect_clicked(move |_| {
            let panel = Rc::clone(&panel);
            let peer = peer.clone();
            glib::spawn_future_local(async move { panel.share(peer).await });
        });
    }

    fn request_stop(&self) {
        if let Some(stop) = self.stop.borrow().as_ref() {
            self.widgets.banner.set_title("Stopping…");
            self.widgets.banner.set_button_label(None);
            stop.store(true, Ordering::Relaxed);
        }
    }

    fn set_controls_enabled(&self, enabled: bool) {
        self.search.set_sensitive(enabled);
        self.resolution.set_sensitive(enabled);
        self.frame_rate.set_sensitive(enabled);
        self.list.set_sensitive(enabled);
        for widget in &self.widgets.locked {
            widget.set_sensitive(enabled);
        }
    }

    /// The whole of one session, from the click to the end of sharing.
    async fn share(self: Rc<Self>, peer: Peer) {
        let Some(monitor) = self.selected.borrow().clone() else {
            self.show_toast("Choose a monitor first.");
            return;
        };
        if !firewall::helper_installed() {
            self.show_setup_dialog();
            return;
        }
        // Ends the search loop, if there is one.
        self.search_generation.set(self.search_generation.get() + 1);
        self.set_controls_enabled(false);

        let stop = Arc::new(AtomicBool::new(false));
        self.stop.replace(Some(Arc::clone(&stop)));
        let banner = &self.widgets.banner;
        banner.set_title(&format!("Preparing to share with {}…", peer.name));
        banner.set_button_label(Some("Stop"));
        banner.set_revealed(true);
        (self.widgets.on_sharing)(true);

        let result = self.run_session(&peer, &monitor, stop).await;
        (self.widgets.on_sharing)(false);
        self.widgets.banner.set_revealed(false);
        self.stop.replace(None);
        if self.close_requested.get() {
            self.widgets.window.close();
            return;
        }
        self.set_controls_enabled(true);
        self.describe(SEARCH_DESCRIPTION);
        match result {
            Ok(()) => self.show_toast("Sharing ended."),
            Err(message) => {
                error!(error = %message, "sharing failed");
                self.show_toast(&message);
            }
        }
    }

    fn show_setup_dialog(&self) {
        let dialog = adw::AlertDialog::builder()
            .heading("One More Setup Step")
            .body(
                "ScreenCopy needs its firewall helper, so that the TV can connect. \
                 Install ScreenCopy from its package, or see “Firewall helper” in the README \
                 when running from source.",
            )
            .build();
        dialog.add_response("close", "Close");
        dialog.present(Some(&self.widgets.window));
    }

    async fn run_session(
        &self,
        peer: &Peer,
        monitor: &Monitor,
        stop: Arc<AtomicBool>,
    ) -> Result<(), String> {
        self.describe("Waiting for administrator approval…");
        let authorized = gio::spawn_blocking(firewall::authorize).await;
        authorized
            .map_err(|_| "The authorization did not finish.".to_owned())?
            .map_err(|err| err.to_string())?;
        if stop.load(Ordering::Relaxed) {
            return Ok(());
        }

        let encoder = gio::spawn_blocking(Encoder::detect)
            .await
            .ok()
            .flatten()
            .ok_or("No working H.264 encoder found.")?;

        self.describe(&format!(
            "Connecting to {}. Accept the request on the TV if it asks.",
            peer.name
        ));
        let client = self.client().await.map_err(|err| err.to_string())?;
        if let Err(err) = client.stop_search().await {
            debug!(error = %err, "cannot stop searching");
        }
        let link = match client.connect(peer, &stop).await {
            Ok(link) => link,
            Err(P2pError::Cancelled) => return Ok(()),
            Err(err) => return Err(err.to_string()),
        };

        let result = self.serve_link(&link, peer, monitor, encoder, stop).await;
        client.disconnect(link).await;
        result
    }

    async fn serve_link(
        &self,
        link: &P2pLink,
        peer: &Peer,
        monitor: &Monitor,
        encoder: Encoder,
        stop: Arc<AtomicBool>,
    ) -> Result<(), String> {
        let settings = self.widgets.settings.get();
        let preference = VideoPreference {
            height: settings.video_height,
            frame_rate: settings.frame_rate,
        };
        let config = ServeConfig {
            interface: link.interface.clone(),
            local_address: link.local_address,
            region: monitor.capture_region(),
            encoder,
            sound_delay_ms: Arc::clone(&self.sound_delay_ms),
            video_preference: preference,
        };
        let (sender, events) = mpsc::channel();
        std::thread::Builder::new()
            .name("wfd-session".to_owned())
            .spawn(move || wfd::serve(&config, &stop, &sender))
            .map_err(|err| format!("Cannot start the session: {err}"))?;

        let banner = &self.widgets.banner;
        self.describe(&format!("Connected to {}, waiting for the TV…", peer.name));

        loop {
            match events.try_recv() {
                Ok(ServeEvent::Session(SessionEvent::Negotiating)) => {
                    self.describe(&format!("Setting up the picture with {}…", peer.name));
                }
                Ok(ServeEvent::Session(SessionEvent::Streaming(mode, sound))) => {
                    banner.set_title(&format!(
                        "Sharing monitor {} with {}",
                        monitor.name(),
                        peer.name
                    ));
                    self.describe(&format!(
                        "Sharing at {} × {}, {} frames per second.",
                        mode.width, mode.height, mode.frame_rate
                    ));
                    if !mode.satisfies(preference) {
                        self.show_toast(&format!(
                            "The TV does not offer the chosen format, so it uses {}p at {} fps.",
                            mode.height, mode.frame_rate
                        ));
                    }
                    match sound {
                        Sound::System => {}
                        Sound::Silent => self.show_toast(
                            "The computer's sound could not be captured, so the TV stays silent.",
                        ),
                        Sound::Absent => {
                            self.show_toast("The TV accepts no sound format ScreenCopy can send.");
                        }
                    }
                }
                Ok(ServeEvent::Session(SessionEvent::Stats(stats))) => {
                    self.describe(&format!(
                        "Sharing: {} fps, longest pause {} ms, {:.1} Mbit/s.",
                        stats.encoded_fps,
                        stats
                            .longest_capture_gap_ms
                            .max(stats.longest_encoded_gap_ms),
                        stats.sent_kbit_per_second as f64 / 1000.0,
                    ));
                }
                Ok(ServeEvent::Ended(result)) => return result,
                Err(TryRecvError::Empty) => glib::timeout_future(EVENT_POLL_INTERVAL).await,
                Err(TryRecvError::Disconnected) => {
                    return Err("The session ended unexpectedly.".to_owned());
                }
            }
        }
    }

    fn show_toast(&self, title: &str) {
        let toast = adw::Toast::builder().title(title).use_markup(false).build();
        self.widgets.toasts.add_toast(toast);
    }
}

/// What to say when a search has ended.
fn search_result(found: usize) -> String {
    match found {
        0 => "No TV found. Is Screen Mirroring on?".to_owned(),
        found if found <= TVS_WITHOUT_SCROLLING => "Choose the TV to share with.".to_owned(),
        found => format!("{found} TVs found. Scroll the list to see them all."),
    }
}

/// A list with the recommended choice first and `current` selected.
fn choice_row(
    title: &str,
    recommended: &str,
    choices: &[u32],
    label: impl Fn(u32) -> String,
    current: Option<u32>,
) -> adw::ComboRow {
    let mut labels = vec![recommended.to_owned()];
    labels.extend(choices.iter().copied().map(label));
    let labels: Vec<&str> = labels.iter().map(String::as_str).collect();
    let row = adw::ComboRow::builder()
        .title(title)
        .model(&gtk::StringList::new(&labels))
        .build();
    let position = current.and_then(|value| choices.iter().position(|choice| *choice == value));
    // A list this short always fits: position 0 is the recommended choice.
    row.set_selected(position.map_or(0, |position| position as u32 + 1));
    row
}

/// The value of the selected entry, or `None` for the recommended one.
fn chosen(row: &adw::ComboRow, choices: &[u32]) -> Option<u32> {
    let position = usize::try_from(row.selected()).ok()?.checked_sub(1)?;
    choices.get(position).copied()
}

/// Lets the user match the sound to the picture while watching. It stays
/// available during a session, and takes effect at once.
fn sound_delay_row(settings: SettingsStore, delay: Arc<AtomicI32>) -> adw::SpinRow {
    let row = adw::SpinRow::with_range(
        f64::from(MIN_SOUND_DELAY_MS),
        f64::from(MAX_SOUND_DELAY_MS),
        SOUND_DELAY_STEP_MS,
    );
    row.set_title("Sound Delay (ms)");
    row.set_subtitle("Raise it if sound comes before the picture, lower it if after.");
    row.set_value(f64::from(settings.get().sound_delay_ms));
    row.connect_value_notify(move |row| {
        // The range above makes this conversion lossless.
        let sound_delay_ms = row.value().round() as i32;
        delay.store(sound_delay_ms, Ordering::Relaxed);
        settings.update(|settings| settings.sound_delay_ms = sound_delay_ms);
    });
    row
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_search_result_mentions_tvs_hidden_by_scrolling() {
        assert!(search_result(0).contains("No TV"));
        assert!(search_result(1).contains("Choose"));
        assert!(search_result(TVS_WITHOUT_SCROLLING).contains("Choose"));
        assert!(search_result(3).contains("3 TVs"));
        assert!(search_result(3).contains("Scroll"));
    }
}
