//! Finding Miracast receivers and sharing the selected monitor with one. A TV's
//! row in the list also shows the state of its session and offers Stop.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::sync::mpsc::{self, TryRecvError};
use std::time::Duration;

use adw::prelude::*;
use gtk::{gio, glib};
use tracing::{debug, error};

use super::section::Section;
use crate::capture::monitor::Monitor;
use crate::capture::recorder::Encoder;
use crate::capture::stream_stats::StreamStats;
use crate::firewall;
use crate::p2p::{P2pClient, P2pError, P2pLink, Peer, SEARCH_SECONDS};
use crate::settings::{
    FRAME_RATES, MAX_SOUND_DELAY_MS, MIN_SOUND_DELAY_MS, SettingsStore, VIDEO_HEIGHTS,
};
use crate::tv_memory::{KnownTv, TvFormat, TvMemory};
use crate::wfd::params::{VideoMode, VideoPreference};
use crate::wfd::session::{SessionEvent, Sound};
use crate::wfd::{self, ServeConfig, ServeEvent};

const REFRESH_INTERVAL: Duration = Duration::from_secs(1);
const SOUND_DELAY_STEP_MS: f64 = 10.0;
const EVENT_POLL_INTERVAL: Duration = Duration::from_millis(100);
const PAGE_MESSAGE: &str = "message";
const PAGE_TVS: &str = "tvs";
const SEARCH_LABEL: &str = "Search";
const SEARCH_HINT: &str = "Turn on Screen Mirroring on the TV, then click Search.";
const SEARCHING: &str = "Searching for TVs…";
const NOTHING_FOUND: &str = "No TV found. Is Screen Mirroring on?";
/// How long to look again for a TV that the last search found. The computer forgets
/// found TVs after a while, and a Share can come much later.
const FIND_AGAIN_SECONDS: u32 = 15;
const RECOMMENDED_FORMAT: TvFormat = TvFormat {
    height: 1080,
    frame_rate: 30,
};
const MESSAGE_ICON_SIZE: i32 = 32;
const MESSAGE_SPACING: i32 = 12;
const MESSAGE_MARGIN: i32 = 18;

/// The widgets `TvPanel` needs from the window that hosts it.
pub struct TvWidgets {
    pub section: Section,
    pub picture_section: Section,
    pub settings: SettingsStore,
    /// Called when sharing starts and ends.
    pub on_sharing: Box<dyn Fn(bool)>,
    pub window: adw::ApplicationWindow,
    /// The window's title, which says when the screen is being shared.
    pub title: adw::WindowTitle,
    pub toasts: adw::ToastOverlay,
    /// Widgets that must not change while sharing.
    pub locked: Vec<gtk::Widget>,
}

/// What the list shows about one TV.
#[derive(Debug, Clone, PartialEq, Eq)]
struct TvEntry {
    hw_address: String,
    name: String,
    peer: Option<Peer>,
}

/// One TV in the list, which also shows the state of its session.
struct TvRow {
    hw_address: String,
    name: String,
    /// `None` for the last used TV until a search finds it.
    peer: Option<Peer>,
    row: adw::ActionRow,
    button: gtk::Button,
    icon: gtk::Image,
    spinner: gtk::Spinner,
}

impl TvRow {
    fn new(entry: TvEntry, searching: bool) -> Self {
        let button = gtk::Button::builder().valign(gtk::Align::Center).build();
        let icon = gtk::Image::from_icon_name("video-display-symbolic");
        let spinner = gtk::Spinner::builder().visible(false).build();
        let state = gtk::Box::default();
        state.append(&icon);
        state.append(&spinner);
        // Names come over the air, so they are not markup.
        let row = adw::ActionRow::builder()
            .use_markup(false)
            .title(&entry.name)
            .title_lines(1)
            .subtitle_lines(1)
            .build();
        row.add_prefix(&state);
        row.add_suffix(&button);
        let this = Self {
            hw_address: entry.hw_address,
            name: entry.name,
            peer: entry.peer,
            row,
            button,
            icon,
            spinner,
        };
        this.show_idle(searching);
        this
    }

    fn show_idle(&self, searching: bool) {
        let (status, available) = match &self.peer {
            Some(peer) if peer.info.available => ("Ready", true),
            Some(_) => ("Busy", false),
            None if searching => ("Looking for this TV…", false),
            None => ("Not found. Is Screen Mirroring on?", false),
        };
        self.show_status(status, self.peer.is_none() && searching);
        self.row.set_visible(true);
        self.icon.remove_css_class("success");
        self.button.set_label("Share");
        self.button.remove_css_class("destructive-action");
        self.button.add_css_class("suggested-action");
        self.button.set_sensitive(available);
    }

    fn show_session(&self) {
        self.button.set_label("Stop");
        self.button.set_sensitive(true);
        self.button.remove_css_class("suggested-action");
        self.button.add_css_class("destructive-action");
    }

    fn show_stopping(&self) {
        self.show_status("Stopping…", true);
        self.icon.remove_css_class("success");
        self.button.set_sensitive(false);
    }

    fn show_status(&self, status: &str, busy: bool) {
        self.row.set_subtitle(status);
        self.row
            .set_tooltip_text(Some(&format!("{}\n{status}", self.name)));
        self.icon.set_visible(!busy);
        self.spinner.set_visible(busy);
        self.spinner.set_spinning(busy);
    }

    fn show_sharing(&self, status: &str) {
        self.show_status(status, false);
        self.icon.add_css_class("success");
    }
}

pub struct TvPanel {
    widgets: TvWidgets,
    selected: Rc<RefCell<Option<Monitor>>>,
    pages: gtk::Stack,
    list: gtk::ListBox,
    message: gtk::Label,
    message_icon: gtk::Image,
    message_spinner: gtk::Spinner,
    search: gtk::Button,
    resolution: adw::ComboRow,
    frame_rate: adw::ComboRow,
    /// What the two rows above offer after their recommended entry.
    heights: RefCell<Vec<u32>>,
    frame_rates: RefCell<Vec<u32>>,
    /// Filling the rows must not count as the user choosing something.
    filling_choices: Cell<bool>,
    memory: RefCell<TvMemory>,
    searching: Cell<bool>,
    client: RefCell<Option<Rc<P2pClient>>>,
    rows: RefCell<Vec<Rc<TvRow>>>,
    /// The row of the running session.
    active: RefCell<Option<Rc<TvRow>>>,
    /// Lets a running search notice that it was replaced or cancelled.
    search_generation: Cell<u64>,
    stop: RefCell<Option<Arc<AtomicBool>>>,
    sound_delay_ms: Arc<AtomicI32>,
    /// The window closes once the running session has cleaned up.
    close_requested: Cell<bool>,
}

impl TvPanel {
    /// Fills in the sections. The buttons' signal handlers keep the panel alive
    /// with the window.
    pub fn attach(widgets: TvWidgets, selected: Rc<RefCell<Option<Monitor>>>) {
        let search = gtk::Button::builder().label(SEARCH_LABEL).build();
        widgets.section.set_suffix(&search);

        let list = gtk::ListBox::builder()
            .selection_mode(gtk::SelectionMode::Single)
            .show_separators(true)
            .css_classes(["tv-list"])
            .build();
        // Further TVs scroll within the card, so that the window keeps its size.
        // The scrollbar stays visible when needed, or they could go unnoticed.
        let tvs = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .overlay_scrolling(false)
            .child(&list)
            .build();

        let message_icon = gtk::Image::builder()
            .icon_name("video-display-symbolic")
            .pixel_size(MESSAGE_ICON_SIZE)
            .css_classes(["dim-label"])
            .build();
        let message_spinner = gtk::Spinner::builder()
            .width_request(MESSAGE_ICON_SIZE)
            .height_request(MESSAGE_ICON_SIZE)
            .visible(false)
            .build();
        let message = gtk::Label::builder()
            .label(SEARCH_HINT)
            .wrap(true)
            .justify(gtk::Justification::Center)
            .css_classes(["dim-label"])
            .build();
        let message_page = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(MESSAGE_SPACING)
            .valign(gtk::Align::Center)
            .margin_start(MESSAGE_MARGIN)
            .margin_end(MESSAGE_MARGIN)
            .build();
        message_page.append(&message_icon);
        message_page.append(&message_spinner);
        message_page.append(&message);

        let pages = gtk::Stack::builder().vexpand(true).build();
        pages.add_named(&message_page, Some(PAGE_MESSAGE));
        pages.add_named(&tvs, Some(PAGE_TVS));
        let tv_card = gtk::Box::builder()
            .css_classes(["card"])
            .overflow(gtk::Overflow::Hidden)
            .build();
        pages.set_hexpand(true);
        tv_card.append(&pages);
        widgets.section.set_content(&tv_card);

        let resolution = adw::ComboRow::builder().title("Resolution").build();
        let frame_rate = adw::ComboRow::builder().title("Frame Rate").build();
        let sound_delay_ms = Arc::new(AtomicI32::new(widgets.settings.get().sound_delay_ms));
        let picture_list = gtk::ListBox::builder()
            .selection_mode(gtk::SelectionMode::None)
            .css_classes(["boxed-list"])
            .valign(gtk::Align::Start)
            .build();
        picture_list.append(&resolution);
        picture_list.append(&frame_rate);
        picture_list.append(&sound_delay_row(
            widgets.settings.clone(),
            Arc::clone(&sound_delay_ms),
        ));
        widgets.picture_section.set_content(&picture_list);

        // The two cards stand side by side, so they have the same height. The
        // widgets keep the group alive.
        let same_height = gtk::SizeGroup::new(gtk::SizeGroupMode::Vertical);
        same_height.add_widget(&tv_card);
        same_height.add_widget(&picture_list);

        let this = Rc::new(Self {
            widgets,
            selected,
            pages,
            list,
            message,
            message_icon,
            message_spinner,
            search,
            resolution,
            frame_rate,
            heights: RefCell::default(),
            frame_rates: RefCell::default(),
            filling_choices: Cell::new(false),
            memory: RefCell::new(TvMemory::load()),
            searching: Cell::new(false),
            client: RefCell::default(),
            rows: RefCell::default(),
            active: RefCell::default(),
            search_generation: Cell::new(0),
            stop: RefCell::default(),
            sound_delay_ms,
            close_requested: Cell::new(false),
        });

        this.show_format_choices();
        let panel = Rc::clone(&this);
        this.list
            .connect_row_selected(move |_, _| panel.show_format_choices());
        let panel = Rc::clone(&this);
        this.resolution.connect_selected_notify(move |row| {
            if !panel.filling_choices.get() {
                let height = chosen(row, &panel.heights.borrow());
                panel
                    .widgets
                    .settings
                    .update(|settings| settings.video_height = height);
                // Each resolution can have its own frame rates.
                panel.show_frame_rate_choices();
            }
        });
        let panel = Rc::clone(&this);
        this.frame_rate.connect_selected_notify(move |row| {
            if !panel.filling_choices.get() {
                let rate = chosen(row, &panel.frame_rates.borrow());
                panel
                    .widgets
                    .settings
                    .update(|settings| settings.frame_rate = rate);
            }
        });

        let panel = Rc::clone(&this);
        this.search.connect_clicked(move |_| {
            let panel = Rc::clone(&panel);
            glib::spawn_future_local(async move { panel.run_search().await });
        });
        // Looking for TVs connects to nothing and captures nothing, and most
        // people open the program in order to find their TV.
        this.search.emit_clicked();
        let panel = Rc::clone(&this);
        let stop_action = gio::ActionEntry::builder("stop-sharing")
            .activate(move |_: &adw::ApplicationWindow, _, _| panel.request_stop())
            .build();
        this.widgets.window.add_action_entries([stop_action]);
        // A session must end properly before the program exits: the sound
        // settings are put back by its thread.
        let panel = Rc::clone(&this);
        this.widgets.window.connect_close_request(move |_| {
            if panel.stop.borrow().is_none() {
                return glib::Propagation::Proceed;
            }
            panel.close_requested.set(true);
            panel.request_stop();
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

    /// Shows a message in the TV card, in place of the list.
    fn show_message(&self, text: &str, searching: bool) {
        self.message.set_text(text);
        self.message_icon.set_visible(!searching);
        self.message_spinner.set_visible(searching);
        self.message_spinner.set_spinning(searching);
        self.pages.set_visible_child_name(PAGE_MESSAGE);
    }

    /// Updates the state shown on the row of the running session.
    fn describe(&self, status: &str) {
        let stopping = self
            .stop
            .borrow()
            .as_ref()
            .is_some_and(|stop| stop.load(Ordering::Relaxed));
        if let (false, Some(tv)) = (stopping, self.active.borrow().as_ref()) {
            tv.show_status(status, true);
        }
    }

    async fn run_search(self: Rc<Self>) {
        let generation = self.search_generation.get() + 1;
        self.search_generation.set(generation);
        self.searching.set(true);
        self.search.set_sensitive(false);
        self.search.set_label("Searching…");
        self.rows.borrow_mut().clear();
        self.list.remove_all();
        self.widgets.section.set_title(&tv_heading(0));
        self.show_message(SEARCHING, true);
        self.show_peers(Vec::new());

        match self.search_until_finished(generation).await {
            Ok(()) => {
                // A newer search or a session has taken over the panel.
                if self.search_generation.get() == generation {
                    self.end_search();
                    if self.rows.borrow().is_empty() {
                        self.show_message(NOTHING_FOUND, false);
                    }
                }
            }
            Err(err) => {
                error!(error = %err, "search failed");
                self.end_search();
                // Not a passing toast: it also says why no TV is listed.
                if self.rows.borrow().is_empty() {
                    self.show_message(&sentence(&err.to_string()), false);
                } else {
                    self.show_toast(&sentence(&err.to_string()));
                }
            }
        }
    }

    fn end_search(&self) {
        self.searching.set(false);
        self.search.set_sensitive(true);
        self.search.set_label(SEARCH_LABEL);
        for tv in self.rows.borrow().iter() {
            tv.show_idle(false);
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
        let entries = tv_entries(self.memory.borrow().last(), peers);
        let same_devices = {
            let rows = self.rows.borrow();
            rows.len() == entries.len()
                && rows.iter().zip(&entries).all(|(row, entry)| {
                    row.hw_address == entry.hw_address
                        && row.name == entry.name
                        && row.peer.is_some() == entry.peer.is_some()
                })
        };
        if same_devices {
            return;
        }
        let selected = self.selected_tv().map(|tv| tv.hw_address.clone());
        self.list.remove_all();
        self.widgets.section.set_title(&tv_heading(entries.len()));
        let rows: Vec<Rc<TvRow>> = entries
            .into_iter()
            .map(|entry| self.add_row(entry))
            .collect();
        if rows.is_empty() {
            self.show_message(SEARCHING, true);
        } else {
            self.pages.set_visible_child_name(PAGE_TVS);
        }
        let to_select = rows
            .iter()
            .find(|tv| Some(&tv.hw_address) == selected.as_ref())
            .or_else(|| rows.first())
            .map(Rc::clone);
        self.rows.replace(rows);
        if let Some(tv) = to_select {
            self.select(&tv);
        }
    }

    fn add_row(self: &Rc<Self>, entry: TvEntry) -> Rc<TvRow> {
        let tv = Rc::new(TvRow::new(entry, self.searching.get()));
        self.list.append(&tv.row);

        let panel = Rc::clone(self);
        let weak_tv = Rc::downgrade(&tv);
        tv.button.connect_clicked(move |_| {
            if panel.stop.borrow().is_some() {
                panel.request_stop();
            } else if let Some(tv) = weak_tv.upgrade() {
                let panel = Rc::clone(&panel);
                glib::spawn_future_local(async move { panel.share(tv).await });
            }
        });
        tv
    }

    fn select(&self, tv: &TvRow) {
        self.list
            .select_row(Some(tv.row.upcast_ref::<gtk::ListBoxRow>()));
    }

    /// The TV whose formats the window shows.
    fn selected_tv(&self) -> Option<Rc<TvRow>> {
        let selected = self.list.selected_row()?;
        self.rows
            .borrow()
            .iter()
            .find(|tv| tv.row.upcast_ref::<gtk::ListBoxRow>() == &selected)
            .cloned()
    }

    /// What the selected TV reported in an earlier session, if anything.
    fn selected_formats(&self) -> Option<Vec<TvFormat>> {
        let tv = self.selected_tv()?;
        let memory = self.memory.borrow();
        let known = memory.find(&tv.hw_address, &tv.name)?;
        (!known.formats.is_empty()).then(|| known.formats.clone())
    }

    /// Offers what the selected TV can show. A TV tells this only during a
    /// connection, so until then the window offers everything.
    fn show_format_choices(&self) {
        let formats = self.selected_formats();
        let heights = heights_for(formats.as_deref());
        let recommended = recommended_format(formats.as_deref());
        self.filling_choices.set(true);
        fill_choices(
            &self.resolution,
            &format!("Recommended ({}p)", recommended.height),
            &heights,
            |height| format!("{height}p"),
            self.widgets.settings.get().video_height,
        );
        self.heights.replace(heights);
        self.filling_choices.set(false);
        self.show_frame_rate_choices();

        self.widgets
            .picture_section
            .set_detail(&match (self.selected_tv(), &formats) {
                (Some(tv), Some(_)) => format!("Formats of {}", tv.name),
                (Some(_), None) => "The TV's formats are known after connecting".to_owned(),
                (None, _) => String::new(),
            });
    }

    fn show_frame_rate_choices(&self) {
        let formats = self.selected_formats();
        let height = chosen(&self.resolution, &self.heights.borrow());
        let rates = frame_rates_for(formats.as_deref(), height);
        let recommended = recommended_format(formats.as_deref());
        self.filling_choices.set(true);
        fill_choices(
            &self.frame_rate,
            &format!("Recommended ({} fps)", recommended.frame_rate),
            &rates,
            |rate| {
                if rate == 60 {
                    "60 fps (can stutter more)".to_owned()
                } else {
                    format!("{rate} fps")
                }
            },
            self.widgets.settings.get().frame_rate,
        );
        self.frame_rates.replace(rates);
        self.filling_choices.set(false);
    }

    fn request_stop(&self) {
        if let Some(stop) = self.stop.borrow().as_ref() {
            if let Some(tv) = self.active.borrow().as_ref() {
                tv.show_stopping();
            }
            stop.store(true, Ordering::Relaxed);
        }
    }

    fn set_controls_enabled(&self, enabled: bool) {
        self.search.set_sensitive(enabled);
        self.resolution.set_sensitive(enabled);
        self.frame_rate.set_sensitive(enabled);
        for widget in &self.widgets.locked {
            widget.set_sensitive(enabled);
        }
    }

    /// Leaves only the TV in use in the list, which then shows its session.
    fn show_session(&self, tv: &Rc<TvRow>) {
        for other in self.rows.borrow().iter() {
            other.row.set_visible(Rc::ptr_eq(other, tv));
        }
        self.widgets.section.set_title(&tv_heading(1));
        self.select(tv);
        tv.show_session();
        self.active.replace(Some(Rc::clone(tv)));
    }

    fn show_no_session(&self) {
        self.active.replace(None);
        {
            let rows = self.rows.borrow();
            for tv in rows.iter() {
                tv.show_idle(false);
            }
            self.widgets.section.set_title(&tv_heading(rows.len()));
        }
        self.widgets.title.set_subtitle("");
        // The session may have reported the TV's formats.
        self.show_format_choices();
    }

    /// The whole of one session, from the click to the end of sharing.
    async fn share(self: Rc<Self>, tv: Rc<TvRow>) {
        let Some(monitor) = self.selected.borrow().clone() else {
            self.show_toast("Choose a monitor first.");
            return;
        };
        // The check below takes a moment, and a second click must not start again.
        tv.button.set_sensitive(false);
        let open_firewall = gio::spawn_blocking(firewall::is_active)
            .await
            .unwrap_or(true);
        if open_firewall && !firewall::helper_installed() {
            tv.show_idle(self.searching.get());
            self.show_setup_dialog();
            return;
        }
        // Ends the search loop, if there is one.
        self.search_generation.set(self.search_generation.get() + 1);
        self.end_search();
        self.set_controls_enabled(false);

        let stop = Arc::new(AtomicBool::new(false));
        self.stop.replace(Some(Arc::clone(&stop)));
        self.show_session(&tv);
        self.describe("Preparing…");
        self.widgets
            .title
            .set_subtitle(&format!("Connecting to {}", tv.name));
        (self.widgets.on_sharing)(true);

        let result = self.run_session(&tv, &monitor, open_firewall, stop).await;
        (self.widgets.on_sharing)(false);
        self.stop.replace(None);
        self.show_no_session();
        if self.close_requested.get() {
            self.widgets.window.close();
            return;
        }
        self.set_controls_enabled(true);
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
                "MonitorBeam needs its firewall helper, so that the TV can connect. \
                 Install MonitorBeam from its package, or see “Firewall helper” in the README \
                 when running from source.",
            )
            .build();
        dialog.add_response("close", "Close");
        dialog.present(Some(&self.widgets.window));
    }

    async fn run_session(
        &self,
        tv: &TvRow,
        monitor: &Monitor,
        open_firewall: bool,
        stop: Arc<AtomicBool>,
    ) -> Result<(), String> {
        if open_firewall {
            self.describe("Waiting for administrator approval…");
            let authorized = gio::spawn_blocking(firewall::authorize).await;
            authorized
                .map_err(|_| "The authorization did not finish.".to_owned())?
                .map_err(|err| err.to_string())?;
            if stop.load(Ordering::Relaxed) {
                return Ok(());
            }
        }

        let encoder = gio::spawn_blocking(Encoder::detect)
            .await
            .ok()
            .flatten()
            .ok_or("No working H.264 encoder found.")?;

        self.describe("Connecting. Accept the request on the TV if it asks.");
        let client = self.client().await.map_err(|err| err.to_string())?;
        let Some(peer) = self.find_again(&client, tv, &stop).await? else {
            return Ok(());
        };
        if let Err(err) = client.stop_search().await {
            debug!(error = %err, "cannot stop searching");
        }
        let link = match client.connect(&peer, &stop).await {
            Ok(link) => link,
            Err(P2pError::Cancelled) => return Ok(()),
            Err(err) => return Err(err.to_string()),
        };

        let result = self
            .serve_link(&link, tv, monitor, encoder, open_firewall, stop)
            .await;
        client.disconnect(link).await;
        result
    }

    /// The TV as NetworkManager knows it now, which may need a new search if the
    /// last one ended a while ago. `None` if the user stopped first.
    async fn find_again(
        &self,
        client: &P2pClient,
        tv: &TvRow,
        stop: &AtomicBool,
    ) -> Result<Option<Peer>, String> {
        let error = |err: P2pError| err.to_string();
        let find = async || -> Result<Option<Peer>, P2pError> {
            let peers = client.peers().await?;
            Ok(peers
                .into_iter()
                .find(|peer| peer.hw_address == tv.hw_address))
        };
        if let Some(peer) = find().await.map_err(error)? {
            return Ok(Some(peer));
        }
        self.describe("Looking for the TV…");
        client.start_search().await.map_err(error)?;
        for _ in 0..FIND_AGAIN_SECONDS {
            glib::timeout_future(REFRESH_INTERVAL).await;
            if stop.load(Ordering::Relaxed) {
                return Ok(None);
            }
            if let Some(peer) = find().await.map_err(error)? {
                return Ok(Some(peer));
            }
        }
        Err("The TV could not be found. Is Screen Mirroring on?".to_owned())
    }

    async fn serve_link(
        &self,
        link: &P2pLink,
        tv: &TvRow,
        monitor: &Monitor,
        encoder: Encoder,
        open_firewall: bool,
        stop: Arc<AtomicBool>,
    ) -> Result<(), String> {
        // What the window shows, which also takes the TV's known formats into account.
        let preference = VideoPreference {
            height: chosen(&self.resolution, &self.heights.borrow()),
            frame_rate: chosen(&self.frame_rate, &self.frame_rates.borrow()),
        };
        let config = ServeConfig {
            interface: link.interface.clone(),
            local_address: link.local_address,
            region: monitor.capture_region(),
            encoder,
            sound_delay_ms: Arc::clone(&self.sound_delay_ms),
            video_preference: preference,
            open_firewall,
        };
        let (sender, events) = mpsc::channel();
        let should_stop = Arc::clone(&stop);
        std::thread::Builder::new()
            .name("wfd-session".to_owned())
            .spawn(move || wfd::serve(&config, &should_stop, &sender))
            .map_err(|err| format!("Cannot start the session: {err}"))?;

        self.describe("Connected, waiting for the TV…");
        let mut format = None;

        loop {
            let event = events.try_recv();
            // Stopping can take a moment, and says so until the session has ended.
            let stopping = stop.load(Ordering::Relaxed);
            match event {
                Ok(ServeEvent::Session(SessionEvent::Negotiating)) if !stopping => {
                    self.describe("Setting up the picture…");
                }
                Ok(ServeEvent::OpeningFirewall) => {
                    self.describe("A firewall may be blocking the TV. Approve opening it.");
                }
                Ok(ServeEvent::Session(SessionEvent::TvFormats(modes))) => {
                    let formats = modes
                        .iter()
                        .map(|mode| TvFormat {
                            height: mode.height,
                            frame_rate: mode.frame_rate,
                        })
                        .collect();
                    self.memory
                        .borrow_mut()
                        .remember(&tv.hw_address, &tv.name, formats);
                }
                Ok(ServeEvent::Session(SessionEvent::Streaming(mode, sound))) => {
                    format = Some(mode);
                    self.widgets.title.set_subtitle(&format!(
                        "Sharing monitor {} with {}",
                        monitor.name(),
                        tv.name
                    ));
                    if !stopping {
                        tv.show_sharing(&format!(
                            "Sharing · {} × {} · {} fps",
                            mode.width, mode.height, mode.frame_rate
                        ));
                    }
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
                            self.show_toast("The TV accepts no sound format MonitorBeam can send.");
                        }
                    }
                }
                Ok(ServeEvent::Session(SessionEvent::Stats(stats))) if !stopping => {
                    if let Some(mode) = format {
                        tv.show_sharing(&sharing_status(mode, &stats));
                    }
                }
                Ok(ServeEvent::Session(_)) => {}
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

/// The TV section's title, which says how many there are when the list could
/// hide some of them.
fn tv_heading(found: usize) -> String {
    if found > 1 {
        format!("TVs ({found})")
    } else {
        "TV".to_owned()
    }
}

/// A running session's state, short enough for one line on the TV's row.
fn sharing_status(mode: VideoMode, stats: &StreamStats) -> String {
    format!(
        "Sharing · {} × {} · {} fps · {:.1} Mbit/s",
        mode.width,
        mode.height,
        stats.encoded_fps,
        stats.sent_kbit_per_second as f64 / 1000.0,
    )
}

/// The list's order: the last used TV first, even if it has not been found yet,
/// then the rest sorted as they were given.
fn tv_entries(last: Option<&KnownTv>, peers: Vec<Peer>) -> Vec<TvEntry> {
    let found = |peer: Peer| TvEntry {
        hw_address: peer.hw_address.clone(),
        name: peer.name.clone(),
        peer: Some(peer),
    };
    let Some(last) = last else {
        return peers.into_iter().map(found).collect();
    };
    // The address first, as two TVs of the same model can share a name.
    let position = peers
        .iter()
        .position(|peer| peer.hw_address == last.hw_address)
        .or_else(|| peers.iter().position(|peer| peer.name == last.name));
    let mut peers = peers;
    let first = match position {
        Some(position) => found(peers.remove(position)),
        None => TvEntry {
            hw_address: last.hw_address.clone(),
            name: last.name.clone(),
            peer: None,
        },
    };
    std::iter::once(first)
        .chain(peers.into_iter().map(found))
        .collect()
}

/// A message that starts lower case, as errors do, as a sentence for the window.
fn sentence(message: &str) -> String {
    let mut characters = message.chars();
    characters.next().map_or_else(String::new, |first| {
        format!("{}{}.", first.to_uppercase(), characters.as_str())
    })
}

/// The format used when nothing else is chosen.
fn recommended_format(formats: Option<&[TvFormat]>) -> TvFormat {
    formats
        .and_then(|formats| formats.first().copied())
        .unwrap_or(RECOMMENDED_FORMAT)
}

/// The resolutions to offer, which are all of them until the TV's are known.
fn heights_for(formats: Option<&[TvFormat]>) -> Vec<u32> {
    VIDEO_HEIGHTS
        .into_iter()
        .filter(|height| {
            formats.is_none_or(|formats| formats.iter().any(|format| format.height == *height))
        })
        .collect()
}

/// The frame rates to offer at `height`, or at any resolution if that is not set.
fn frame_rates_for(formats: Option<&[TvFormat]>, height: Option<u32>) -> Vec<u32> {
    FRAME_RATES
        .into_iter()
        .filter(|rate| {
            formats.is_none_or(|formats| {
                formats.iter().any(|format| {
                    format.frame_rate == *rate
                        && height.is_none_or(|wanted| wanted == format.height)
                })
            })
        })
        .collect()
}

/// Fills a list with the recommended choice first and `current` selected, if the
/// list has it.
fn fill_choices(
    row: &adw::ComboRow,
    recommended: &str,
    choices: &[u32],
    label: impl Fn(u32) -> String,
    current: Option<u32>,
) {
    let mut labels = vec![recommended.to_owned()];
    labels.extend(choices.iter().copied().map(label));
    let labels: Vec<&str> = labels.iter().map(String::as_str).collect();
    row.set_model(Some(&gtk::StringList::new(&labels)));
    let position = current.and_then(|value| choices.iter().position(|choice| *choice == value));
    // A list this short always fits: position 0 is the recommended choice.
    row.set_selected(position.map_or(0, |position| position as u32 + 1));
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
    row.set_subtitle("Raise it if the sound comes first, lower it if it comes late.");
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
    use crate::p2p::wfd_ie::parse_device_info;

    const FORMATS: [TvFormat; 2] = [
        TvFormat {
            height: 720,
            frame_rate: 30,
        },
        TvFormat {
            height: 1080,
            frame_rate: 60,
        },
    ];

    fn known(address: &str, name: &str) -> KnownTv {
        KnownTv {
            hw_address: address.to_owned(),
            name: name.to_owned(),
            formats: Vec::new(),
        }
    }

    fn names(entries: &[TvEntry]) -> Vec<(&str, bool)> {
        entries
            .iter()
            .map(|entry| (entry.name.as_str(), entry.peer.is_some()))
            .collect()
    }

    fn peer(address: &str, name: &str) -> Peer {
        let info = parse_device_info(&[0, 0, 6, 1, 0x11, 0x1C, 0x44, 0, 0x36]).unwrap();
        Peer::for_test(address, name, info)
    }

    #[test]
    fn the_heading_counts_tvs_when_there_are_several() {
        assert_eq!(tv_heading(0), "TV");
        assert_eq!(tv_heading(1), "TV");
        assert_eq!(tv_heading(4), "TVs (4)");
    }

    #[test]
    fn the_last_tv_waits_at_the_top_until_it_is_found() {
        let last = known("aa", "Last");
        assert_eq!(
            names(&tv_entries(Some(&last), vec![peer("bb", "Other")])),
            [("Last", false), ("Other", true)]
        );
    }

    #[test]
    fn the_last_tv_moves_to_the_top_when_found() {
        let last = known("aa", "Last");
        let peers = vec![peer("bb", "Other"), peer("aa", "Last")];
        assert_eq!(
            names(&tv_entries(Some(&last), peers)),
            [("Last", true), ("Other", true)]
        );
    }

    #[test]
    fn the_last_tv_is_recognized_by_name_when_its_address_changed() {
        let last = known("aa", "Last");
        let entries = tv_entries(Some(&last), vec![peer("cc", "Last")]);
        assert_eq!(names(&entries), [("Last", true)]);
        assert_eq!(entries[0].hw_address, "cc");
    }

    #[test]
    fn without_a_last_tv_the_list_shows_what_was_found() {
        assert_eq!(
            names(&tv_entries(None, vec![peer("aa", "A"), peer("bb", "B")])),
            [("A", true), ("B", true)]
        );
        assert!(tv_entries(None, Vec::new()).is_empty());
    }

    #[test]
    fn everything_is_offered_until_the_formats_are_known() {
        assert_eq!(heights_for(None), VIDEO_HEIGHTS);
        assert_eq!(frame_rates_for(None, Some(720)), FRAME_RATES);
        assert_eq!(recommended_format(None), RECOMMENDED_FORMAT);
    }

    #[test]
    fn only_what_the_tv_reported_is_offered() {
        assert_eq!(heights_for(Some(&FORMATS)), [1080, 720]);
        assert_eq!(frame_rates_for(Some(&FORMATS), Some(720)), [30]);
        assert_eq!(frame_rates_for(Some(&FORMATS), Some(1080)), [60]);
        assert_eq!(frame_rates_for(Some(&FORMATS), None), [30, 60]);
        assert_eq!(heights_for(Some(&FORMATS[..1])), [720]);
    }

    #[test]
    fn the_recommendation_is_the_tvs_best_format() {
        assert_eq!(recommended_format(Some(&FORMATS)), FORMATS[0]);
    }

    #[test]
    fn an_error_becomes_a_sentence() {
        assert_eq!(sentence("no device"), "No device.");
        assert_eq!(sentence(""), "");
    }
}
