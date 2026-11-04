//! The always visible playlist of video files with their subtitles, and the controls
//! for playing them. It takes the monitors' place in the window, so that the window keeps its size.

use std::cell::{Cell, RefCell};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use adw::prelude::*;
use gtk::{gdk, gio, glib};
use tracing::error;

use super::tv::sentence;
use crate::capture::file_stream::{
    FileInfo, Playback, PlaybackStatus, SubtitlePosition, SubtitleSize, read_file_info,
};
use crate::settings::{MAX_FILM_VOLUME_PERCENT, SettingsStore};
use crate::subtitles::Subtitles;
use crate::watch_positions::WatchPositions;

const CONTROLS_REFRESH: Duration = Duration::from_millis(250);
/// A drag sends many positions. The film only goes to the one it stops at.
const SEEK_SETTLE: Duration = Duration::from_millis(200);
const POSITION_SAVE_INTERVAL: Duration = Duration::from_secs(5);
const SHORTCUT_SEEK_MS: u64 = 10_000;
const SHORTCUT_VOLUME_STEP: f64 = 0.05;
const SUBTITLE_EXTENSION: &str = "srt";
const MAX_SUBTITLE_DELAY_SECONDS: f64 = 60.0;
const SUBTITLE_DELAY_STEP_SECONDS: f64 = 0.1;
const MILLISECONDS_PER_SECOND: f64 = 1000.0;
const PERCENT: f64 = 100.0;
const CONTROLS_SPACING: i32 = 12;
const CONTROLS_MARGIN: i32 = 12;
const PLAYLIST_WIDTH: i32 = 360;
const PLAYLIST_HEADER_HEIGHT: i32 = 46;

const SUBTITLE_SIZES: [(&str, SubtitleSize); 3] = [
    ("Small", SubtitleSize::Small),
    ("Medium", SubtitleSize::Medium),
    ("Large", SubtitleSize::Large),
];
const SUBTITLE_POSITIONS: [(&str, SubtitlePosition); 2] = [
    ("Bottom", SubtitlePosition::Bottom),
    ("Higher", SubtitlePosition::Higher),
];
const PLAY_ICON: &str = "media-playback-start-symbolic";
const PAUSE_ICON: &str = "media-playback-pause-symbolic";
const VOLUME_ICONS: [&str; 4] = [
    "audio-volume-muted-symbolic",
    "audio-volume-high-symbolic",
    "audio-volume-low-symbolic",
    "audio-volume-medium-symbolic",
];
const NO_FILE_TITLE: &str = "No Video File";
const NO_FILE_HINT: &str = "Add files to the playlist.";
const EMPTY_PLAYLIST: &str = "Add video files, or drop them here. They play one after another.";

const NO_SUBTITLES: &str = "None";

/// Told whether a film plays, or `None` if no film does.
type PlayingCallback = Box<dyn Fn(Option<bool>)>;

/// One film in the playlist.
#[derive(Clone)]
struct Film {
    path: PathBuf,
    name: String,
    info: FileInfo,
    subtitles: Option<Arc<Subtitles>>,
    subtitle_name: Option<String>,
    audio_track: u32,
}

/// What the TV panel needs to share the playlist.
pub struct ChosenFile {
    pub playback: Arc<Playback>,
}

pub struct VideoFilePanel {
    /// Goes where the monitors would be.
    pub root: gtk::Box,
    window: adw::ApplicationWindow,
    toasts: adw::ToastOverlay,
    file_row: adw::ActionRow,
    add_files: gtk::Button,
    previous: gtk::Button,
    next: gtk::Button,
    playlist: gtk::ListBox,
    playlist_count: gtk::Label,

    track_row: adw::ComboRow,
    filling_tracks: Cell<bool>,
    subtitle_row: adw::ActionRow,
    remove_subtitles: gtk::Button,
    size: gtk::DropDown,
    position: gtk::DropDown,
    delay: gtk::SpinButton,
    play: gtk::Button,
    back: gtk::Button,
    forward: gtk::Button,
    scale: gtk::Scale,
    time: gtk::Label,
    volume: gtk::ScaleButton,
    settings: SettingsStore,
    playback: Arc<Playback>,
    films: RefCell<Vec<Film>>,
    current: Cell<Option<usize>>,
    positions: RefCell<WatchPositions>,
    pending_seek: Cell<Option<(f64, Instant)>>,
    position_saved: Cell<Instant>,
    /// Where the film will go on from, to say so once it plays.
    resuming_at: Cell<Option<u64>>,
    /// Told whether a film plays, or that none does, when that changes.
    on_playing: RefCell<Option<PlayingCallback>>,
    last_playing: Cell<Option<bool>>,
    was_running: Cell<bool>,
    /// Stays set for the time the file is being read, to read them in order.
    reading: Cell<bool>,
    waiting_files: RefCell<Vec<PathBuf>>,
}

/// `1:02:03`, or `2:03` for less than an hour.
fn format_time(milliseconds: u64) -> String {
    let seconds = milliseconds / 1000;
    let (hours, minutes, seconds) = (seconds / 3600, seconds / 60 % 60, seconds % 60);
    if hours > 0 {
        format!("{hours}:{minutes:02}:{seconds:02}")
    } else {
        format!("{minutes}:{seconds:02}")
    }
}

fn time_text(position_ms: u64, duration_ms: u64) -> String {
    format!(
        "{} / {}",
        format_time(position_ms),
        format_time(duration_ms)
    )
}

/// The name of a path, for showing. Paths need not be valid text.
fn display_name(path: &Path) -> String {
    path.file_name()
        .map_or_else(String::new, |name| name.to_string_lossy().into_owned())
}

/// Subtitles usually lie next to a film, under the same name.
fn subtitles_next_to(video: &Path) -> Option<PathBuf> {
    let path = video.with_extension(SUBTITLE_EXTENSION);
    path.is_file().then_some(path)
}

fn file_summary(info: &FileInfo) -> String {
    let duration = format_time(info.duration_ms);
    match info.audio_tracks.len() {
        0 => format!("{duration} · No sound"),
        1 => duration,
        tracks => format!("{duration} · {tracks} sound tracks"),
    }
}

/// `2 of 5 · 1:32:05`, or only the summary for a single film.
fn playlist_summary(index: usize, count: usize, info: &FileInfo) -> String {
    if count > 1 {
        format!("{} of {count} · {}", index + 1, file_summary(info))
    } else {
        file_summary(info)
    }
}

/// A film's length and subtitles, for its row in the playlist.
fn playlist_row_details(film: &Film) -> String {
    let duration = format_time(film.info.duration_ms);
    match &film.subtitle_name {
        Some(subtitles) => format!("{duration} · {subtitles}"),
        None => format!("{duration} · No subtitles"),
    }
}

/// The position after a step, within the film.
fn stepped_position(status: PlaybackStatus, forward: bool) -> u64 {
    if forward {
        let wanted = status.position_ms + SHORTCUT_SEEK_MS;
        if status.duration_ms > 0 {
            wanted.min(status.duration_ms)
        } else {
            wanted
        }
    } else {
        status.position_ms.saturating_sub(SHORTCUT_SEEK_MS)
    }
}

fn choice_list(names: &[&str], selected: usize) -> gtk::DropDown {
    let list = gtk::DropDown::from_strings(names);
    // Each list is far shorter than the conversion's limit.
    list.set_selected(selected as u32);
    list.set_valign(gtk::Align::Center);
    list
}

fn flat_button(icon: &str, tooltip: &str) -> gtk::Button {
    gtk::Button::builder()
        .icon_name(icon)
        .tooltip_text(tooltip)
        .valign(gtk::Align::Center)
        .css_classes(["flat"])
        .sensitive(false)
        .build()
}

impl VideoFilePanel {
    pub fn new(
        window: &adw::ApplicationWindow,
        toasts: &adw::ToastOverlay,
        settings: &SettingsStore,
    ) -> Rc<Self> {
        let add_files = gtk::Button::builder()
            .label("Add…")
            .tooltip_text("Add Video Files to the Playlist")
            .valign(gtk::Align::Center)
            .css_classes(["suggested-action"])
            .build();
        let previous = flat_button("media-skip-backward-symbolic", "Previous File");
        let next = flat_button("media-skip-forward-symbolic", "Next File");
        let playlist = gtk::ListBox::builder()
            .selection_mode(gtk::SelectionMode::None)
            .show_separators(true)
            .css_classes(["tv-list"])
            .build();
        playlist.set_placeholder(Some(
            &gtk::Label::builder()
                .label(EMPTY_PLAYLIST)
                .wrap(true)
                .justify(gtk::Justification::Center)
                .margin_start(CONTROLS_MARGIN)
                .margin_end(CONTROLS_MARGIN)
                .css_classes(["dim-label"])
                .build(),
        ));
        // Files beyond what fits scroll here. The scrollbar stays visible when needed.
        let playlist_scroll = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .overlay_scrolling(false)
            .vexpand(true)
            .child(&playlist)
            .build();
        let playlist_count = gtk::Label::builder()
            .xalign(0.0)
            .hexpand(true)
            .css_classes(["dim-label"])
            .build();
        let playlist_header = gtk::Box::builder()
            .spacing(CONTROLS_SPACING)
            .height_request(PLAYLIST_HEADER_HEIGHT)
            .margin_start(CONTROLS_MARGIN)
            .margin_end(CONTROLS_MARGIN)
            .build();
        playlist_header.append(
            &gtk::Label::builder()
                .label("Playlist")
                .css_classes(["heading"])
                .build(),
        );
        playlist_header.append(&playlist_count);
        playlist_header.append(&add_files);
        let playlist_side = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .width_request(PLAYLIST_WIDTH)
            .build();
        playlist_side.append(&playlist_header);
        playlist_side.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
        playlist_side.append(&playlist_scroll);

        // File names are not markup.
        let file_row = adw::ActionRow::builder()
            .use_markup(false)
            .title(NO_FILE_TITLE)
            .subtitle(NO_FILE_HINT)
            .title_lines(1)
            .subtitle_lines(1)
            .build();
        file_row.add_suffix(&previous);
        file_row.add_suffix(&next);

        let track_row = adw::ComboRow::builder()
            .use_markup(false)
            .title("Sound Track")
            .sensitive(false)
            .build();

        let choose_subtitles = gtk::Button::builder()
            .label("Choose…")
            .valign(gtk::Align::Center)
            .build();
        let remove_subtitles = flat_button("edit-clear-symbolic", "Remove Subtitles");
        let subtitle_row = adw::ActionRow::builder()
            .use_markup(false)
            .title("Subtitles")
            .subtitle(NO_SUBTITLES)
            .title_lines(1)
            .subtitle_lines(1)
            .build();
        subtitle_row.add_suffix(&remove_subtitles);
        subtitle_row.add_suffix(&choose_subtitles);

        let size_names: Vec<&str> = SUBTITLE_SIZES.iter().map(|(name, _)| *name).collect();
        let position_names: Vec<&str> = SUBTITLE_POSITIONS.iter().map(|(name, _)| *name).collect();
        let saved = settings.get();
        let size = choice_list(
            &size_names,
            SUBTITLE_SIZES
                .iter()
                .position(|(_, size)| *size == saved.subtitle_size)
                .unwrap_or(1),
        );
        size.set_tooltip_text(Some("Subtitle Size"));
        let position = choice_list(
            &position_names,
            SUBTITLE_POSITIONS
                .iter()
                .position(|(_, position)| *position == saved.subtitle_position)
                .unwrap_or(0),
        );
        position.set_tooltip_text(Some("Subtitle Position"));
        let delay = gtk::SpinButton::with_range(
            -MAX_SUBTITLE_DELAY_SECONDS,
            MAX_SUBTITLE_DELAY_SECONDS,
            SUBTITLE_DELAY_STEP_SECONDS,
        );
        delay.set_digits(1);
        delay.set_value(0.0);
        delay.set_valign(gtk::Align::Center);
        delay.set_tooltip_text(Some(
            "Subtitle Delay in Seconds. Raise it if they come too early.",
        ));
        let style_row = adw::ActionRow::builder()
            .title("Subtitle Style")
            .subtitle("Size, position, delay (s)")
            .title_lines(1)
            .subtitle_lines(1)
            .build();
        style_row.add_suffix(&size);
        style_row.add_suffix(&position);
        style_row.add_suffix(&delay);

        let play = gtk::Button::builder()
            .icon_name(PLAY_ICON)
            .tooltip_text("Play or Pause (Space)")
            .valign(gtk::Align::Center)
            .sensitive(false)
            .css_classes(["circular"])
            .build();
        let back = flat_button(
            "media-seek-backward-symbolic",
            "Back 10 Seconds (Left Arrow)",
        );
        let forward = flat_button(
            "media-seek-forward-symbolic",
            "Forward 10 Seconds (Right Arrow)",
        );
        let scale = gtk::Scale::builder()
            .hexpand(true)
            .draw_value(false)
            .sensitive(false)
            .build();
        let time = gtk::Label::builder()
            .label(format_time(0))
            .css_classes(["numeric", "dim-label"])
            .build();
        let max_volume = f64::from(MAX_FILM_VOLUME_PERCENT) / PERCENT;
        let volume = gtk::ScaleButton::new(0.0, max_volume, SHORTCUT_VOLUME_STEP, &VOLUME_ICONS);
        volume.set_value(f64::from(saved.film_volume_percent) / PERCENT);
        volume.set_valign(gtk::Align::Center);
        volume.set_tooltip_text(Some("Film Volume (Up and Down Arrows)"));
        let controls = gtk::Box::builder()
            .spacing(CONTROLS_SPACING)
            .margin_top(CONTROLS_MARGIN / 2)
            .margin_bottom(CONTROLS_MARGIN / 2)
            .margin_start(CONTROLS_MARGIN)
            .margin_end(CONTROLS_MARGIN)
            .build();
        controls.append(&back);
        controls.append(&play);
        controls.append(&forward);
        controls.append(&scale);

        controls.append(&time);
        controls.append(&volume);
        let settings_list = gtk::ListBox::builder()
            .selection_mode(gtk::SelectionMode::None)
            .show_separators(true)
            .hexpand(true)
            .css_classes(["tv-list"])
            .build();
        settings_list.append(&file_row);
        settings_list.append(&track_row);
        settings_list.append(&subtitle_row);
        settings_list.append(&style_row);

        let columns = gtk::Box::builder().vexpand(true).build();
        columns.append(&settings_list);
        columns.append(&gtk::Separator::new(gtk::Orientation::Vertical));
        columns.append(&playlist_side);
        let root = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .build();
        root.append(&columns);
        root.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
        root.append(&controls);

        let playback = Arc::new(Playback::default());
        playback.set_volume(f64::from(saved.film_volume_percent) / PERCENT);
        let this = Rc::new(Self {
            root,
            window: window.clone(),
            toasts: toasts.clone(),
            file_row,
            add_files,
            previous,
            next,
            playlist,
            playlist_count,

            track_row,
            filling_tracks: Cell::new(false),
            subtitle_row,
            remove_subtitles,
            size,
            position,
            delay,
            play,
            back,
            forward,
            scale,
            time,
            volume,
            settings: settings.clone(),
            playback,
            films: RefCell::default(),
            current: Cell::new(None),
            positions: RefCell::new(WatchPositions::load()),
            pending_seek: Cell::new(None),
            position_saved: Cell::new(Instant::now()),
            resuming_at: Cell::new(None),
            on_playing: RefCell::default(),
            last_playing: Cell::new(None),
            was_running: Cell::new(false),
            reading: Cell::new(false),
            waiting_files: RefCell::default(),
        });
        this.apply_style();
        this.connect_signals(&choose_subtitles);
        this
    }

    fn connect_signals(self: &Rc<Self>, choose_subtitles: &gtk::Button) {
        let this = Rc::clone(self);
        self.add_files
            .connect_clicked(move |_| this.ask_for_videos());
        let this = Rc::clone(self);
        self.previous
            .connect_clicked(move |_| this.step_film(false));
        let this = Rc::clone(self);
        self.next.connect_clicked(move |_| this.step_film(true));
        let this = Rc::clone(self);
        choose_subtitles.connect_clicked(move |_| this.ask_for_subtitles());
        let this = Rc::clone(self);
        self.remove_subtitles
            .connect_clicked(move |_| this.set_subtitles(None, None));

        let this = Rc::clone(self);
        self.track_row.connect_selected_notify(move |row| {
            if this.filling_tracks.get() {
                return;
            }
            let selected = row.selected();
            if let Ok(track) = i32::try_from(selected) {
                this.playback.set_audio_track(track);
            }
            if let (Some(index), Ok(selected_index)) =
                (this.current.get(), usize::try_from(selected))
                && selected_index < row.model().map_or(0, |model| model.n_items() as usize)
                && let Some(film) = this.films.borrow_mut().get_mut(index)
            {
                film.audio_track = selected;
            }
        });
        let this = Rc::clone(self);
        self.size.connect_selected_notify(move |list| {
            let size = SUBTITLE_SIZES
                .get(list.selected() as usize)
                .map_or_else(SubtitleSize::default, |(_, size)| *size);
            this.settings
                .update(|settings| settings.subtitle_size = size);
            this.apply_style();
        });
        let this = Rc::clone(self);
        self.position.connect_selected_notify(move |list| {
            let position = SUBTITLE_POSITIONS
                .get(list.selected() as usize)
                .map_or_else(SubtitlePosition::default, |(_, position)| *position);
            this.settings
                .update(|settings| settings.subtitle_position = position);
            this.apply_style();
        });
        let this = Rc::clone(self);
        self.delay.connect_value_changed(move |_| {
            this.apply_style();
            // The field keeps the keyboard otherwise, and the shortcuts stop working.
            gtk::prelude::GtkWindowExt::set_focus(&this.window, None::<&gtk::Widget>);
        });
        let this = Rc::clone(self);
        self.back
            .connect_clicked(move |_| this.step_within_film(false));
        let this = Rc::clone(self);
        self.forward
            .connect_clicked(move |_| this.step_within_film(true));

        let this = Rc::clone(self);
        self.play.connect_clicked(move |_| this.play_or_pause());
        let this = Rc::clone(self);
        self.scale.connect_change_value(move |_, _, value| {
            this.pending_seek.set(Some((value, Instant::now())));
            glib::Propagation::Proceed
        });
        let this = Rc::clone(self);
        self.volume.connect_value_changed(move |_, volume| {
            this.playback.set_volume(volume);
            // The range is limited to what fits.
            let percent = (volume * PERCENT).round() as u32;
            this.settings
                .update(|settings| settings.film_volume_percent = percent);
        });

        let drop_target = gtk::DropTarget::new(gdk::FileList::static_type(), gdk::DragAction::COPY);
        let this = Rc::clone(self);
        drop_target.connect_drop(move |_, value, _, _| {
            let Ok(files) = value.get::<gdk::FileList>() else {
                return false;
            };
            // A recording locks the panel.
            if !this.root.is_sensitive() {
                return false;
            }
            this.add_videos(files.files().iter().filter_map(FileExt::path).collect());
            true
        });
        self.root.add_controller(drop_target);

        self.connect_shortcuts();

        let this = Rc::clone(self);
        let play_or_pause = gio::ActionEntry::builder("play-or-pause")
            .activate(move |_: &adw::ApplicationWindow, _, _| this.play_or_pause())
            .build();
        self.window.add_action_entries([play_or_pause]);

        // The strong reference keeps the panel alive with the window.
        let this = Rc::clone(self);
        glib::timeout_add_local(CONTROLS_REFRESH, move || {
            this.refresh_controls();
            glib::ControlFlow::Continue
        });
    }

    /// Only while a film plays, and never while something is being typed.
    fn connect_shortcuts(self: &Rc<Self>) {
        let keys = gtk::EventControllerKey::new();
        keys.set_propagation_phase(gtk::PropagationPhase::Capture);
        let this = Rc::clone(self);
        keys.connect_key_pressed(move |_, key, _, _| {
            let typing = gtk::prelude::GtkWindowExt::focus(&this.window)
                .is_some_and(|focus| focus.is::<gtk::Text>());
            let status = this.playback.status();
            if !status.running || typing {
                return glib::Propagation::Proceed;
            }
            tracing::debug!(?key, "shortcut key");
            match key {
                gdk::Key::space => this.play_or_pause(),
                gdk::Key::Left => this.step_within_film(false),
                gdk::Key::Right => this.step_within_film(true),

                gdk::Key::Up => this
                    .volume
                    .set_value(this.volume.value() + SHORTCUT_VOLUME_STEP),
                gdk::Key::Down => this
                    .volume
                    .set_value(this.volume.value() - SHORTCUT_VOLUME_STEP),
                _ => return glib::Propagation::Proceed,
            }
            glib::Propagation::Stop
        });
        self.window.add_controller(keys);
    }

    /// Gets told whether a film plays, or that none does.
    pub fn connect_playing(&self, on_playing: impl Fn(Option<bool>) + 'static) {
        self.on_playing.replace(Some(Box::new(on_playing)));
    }

    /// The playlist to share, if it has anything, ready to start. A film goes on from
    /// where it was left.
    pub fn prepare_to_share(&self) -> Option<ChosenFile> {
        let index = self.current.get()?;
        let film = self.films.borrow().get(index)?.clone();
        self.start_film(&film);
        Some(ChosenFile {
            playback: Arc::clone(&self.playback),
        })
    }

    fn start_film(&self, film: &Film) {
        self.playback.load(film.path.clone());
        self.playback.set_subtitles(film.subtitles.clone());
        if let Ok(track) = i32::try_from(film.audio_track) {
            self.playback.set_audio_track(track);
        }
        let position = self.positions.borrow().position_of(&film.path);
        if let Some(position) = position {
            self.playback.seek(position);
        }
        self.resuming_at.set(position);
    }

    /// Keeps the position of a session that has ended.
    pub fn session_ended(&self) {
        self.remember_position(self.playback.status());
        self.scale.set_value(0.0);
        self.playback.set_paused(false);
        self.show_time(0);
    }

    fn show_time(&self, position_ms: u64) {
        let duration_ms = self.current_film().map_or(0, |film| film.info.duration_ms);
        self.time.set_text(&time_text(position_ms, duration_ms));
    }

    fn show_toast(&self, title: &str) {
        let toast = adw::Toast::builder().title(title).use_markup(false).build();
        self.toasts.add_toast(toast);
    }

    fn current_film(&self) -> Option<Film> {
        self.films.borrow().get(self.current.get()?).cloned()
    }

    fn ask_for_videos(self: &Rc<Self>) {
        let videos = gtk::FileFilter::new();
        videos.set_name(Some("Video Files"));
        videos.add_mime_type("video/*");
        let everything = gtk::FileFilter::new();
        everything.set_name(Some("All Files"));
        everything.add_pattern("*");
        let filters = gio::ListStore::new::<gtk::FileFilter>();
        filters.append(&videos);
        filters.append(&everything);
        let dialog = gtk::FileDialog::builder()
            .title("Add Video Files")
            .modal(true)
            .filters(&filters)
            .build();
        if let Some(folder) = glib::user_special_dir(glib::UserDirectory::Videos) {
            dialog.set_initial_folder(Some(&gio::File::for_path(folder)));
        }
        let this = Rc::clone(self);
        dialog.open_multiple(
            Some(&self.window),
            gio::Cancellable::NONE,
            move |result| match result {
                Ok(files) => {
                    let paths = (0..files.n_items())
                        .filter_map(|index| files.item(index))
                        .filter_map(|file| file.downcast::<gio::File>().ok())
                        .filter_map(|file| file.path())
                        .collect();
                    this.add_videos(paths);
                }
                Err(err) if err.matches(gtk::DialogError::Dismissed) => {}
                Err(err) => {
                    error!(error = %err, "file selection failed");
                    this.show_toast("Cannot open the file selection.");
                }
            },
        );
    }

    fn ask_for_subtitles(self: &Rc<Self>) {
        let subtitles = gtk::FileFilter::new();
        subtitles.set_name(Some("Subtitles (SRT)"));
        subtitles.add_suffix(SUBTITLE_EXTENSION);
        let filters = gio::ListStore::new::<gtk::FileFilter>();
        filters.append(&subtitles);
        let dialog = gtk::FileDialog::builder()
            .title("Choose Subtitles")
            .modal(true)
            .filters(&filters)
            .build();
        let this = Rc::clone(self);
        dialog.open(
            Some(&self.window),
            gio::Cancellable::NONE,
            move |result| match result {
                Ok(file) => match file.path() {
                    Some(path) => this.open_subtitles(&path),
                    None => this.show_toast("Choose a file on this computer."),
                },
                Err(err) if err.matches(gtk::DialogError::Dismissed) => {}
                Err(err) => {
                    error!(error = %err, "file selection failed");
                    this.show_toast("Cannot open the file selection.");
                }
            },
        );
    }

    /// Adds files in the given order. Each is read on another thread, one at a time,
    /// because a bad one can take a while.
    fn add_videos(self: &Rc<Self>, paths: Vec<PathBuf>) {
        self.waiting_files.borrow_mut().extend(paths);
        if self.reading.get() {
            return;
        }
        self.reading.set(true);
        let this = Rc::clone(self);
        glib::spawn_future_local(async move {
            loop {
                let path = {
                    let mut waiting = this.waiting_files.borrow_mut();
                    if waiting.is_empty() {
                        break;
                    }
                    waiting.remove(0)
                };
                if this.films.borrow().is_empty() {
                    this.file_row.set_title(&display_name(&path));
                    this.file_row.set_subtitle("Reading the file…");
                }
                let to_read = path.clone();
                let info = gio::spawn_blocking(move || read_file_info(&to_read))
                    .await
                    .unwrap_or_else(|_| Err("This file cannot be read.".to_owned()));
                match info {
                    Ok(info) => this.add_film(path, info),
                    Err(message) => {
                        this.show_toast(&format!("{}: {message}", display_name(&path)));
                        this.show_current();
                    }
                }
            }
            this.reading.set(false);
        });
    }

    fn add_film(self: &Rc<Self>, path: PathBuf, info: FileInfo) {
        let mut film = Film {
            name: display_name(&path),
            path,
            info,
            subtitles: None,
            subtitle_name: None,
            audio_track: 0,
        };
        if let Some(subtitles) = subtitles_next_to(&film.path) {
            match Subtitles::load(&subtitles) {
                Ok(loaded) => {
                    film.subtitles = Some(Arc::new(loaded));
                    film.subtitle_name = Some(display_name(&subtitles));
                }
                Err(err) => {
                    error!(error = %err, "cannot use the subtitles next to the film");
                    self.show_toast(&sentence(&err.to_string()));
                }
            }
        }
        self.films.borrow_mut().push(film);
        if self.current.get().is_none() {
            self.current.set(Some(0));
        }
        self.show_current();
        self.rebuild_playlist();
    }

    /// Shows the current film in the rows and the playlist's controls.
    fn show_current(&self) {
        let films = self.films.borrow();
        let index = self.current.get().unwrap_or(0);
        let count = films.len();
        self.previous.set_sensitive(index > 0);
        self.next.set_sensitive(index + 1 < count);
        self.playlist_count.set_text(&if count > 0 {
            format!("{count}")
        } else {
            String::new()
        });

        let Some(film) = films.get(index) else {
            self.file_row.set_title(NO_FILE_TITLE);
            self.file_row.set_subtitle(NO_FILE_HINT);
            self.file_row.set_tooltip_text(None);
            self.subtitle_row.set_subtitle(NO_SUBTITLES);
            self.remove_subtitles.set_sensitive(false);
            self.track_row.set_sensitive(false);
            self.filling_tracks.set(true);
            self.track_row.set_model(gio::ListModel::NONE);
            self.filling_tracks.set(false);
            self.time.set_text(&format_time(0));
            self.scale.set_range(0.0, 0.0);
            return;
        };
        self.file_row.set_title(&film.name);
        self.file_row
            .set_subtitle(&playlist_summary(index, count, &film.info));
        self.file_row.set_tooltip_text(Some(&film.name));
        self.subtitle_row
            .set_subtitle(film.subtitle_name.as_deref().unwrap_or(NO_SUBTITLES));
        self.remove_subtitles
            .set_sensitive(film.subtitles.is_some());

        let tracks: Vec<&str> = film.info.audio_tracks.iter().map(String::as_str).collect();
        self.filling_tracks.set(true);
        self.track_row
            .set_model(Some(&gtk::StringList::new(&tracks)));
        self.track_row.set_selected(film.audio_track);
        self.filling_tracks.set(false);
        self.track_row.set_sensitive(tracks.len() > 1);
        if !self.playback.status().running {
            self.scale
                .set_range(0.0, film.info.duration_ms as f64 / MILLISECONDS_PER_SECOND);
            self.scale.set_value(0.0);
            self.time.set_text(&time_text(0, film.info.duration_ms));
        }
    }

    /// Lists the films, with a way to jump to each one or take it away.
    fn rebuild_playlist(self: &Rc<Self>) {
        self.playlist.remove_all();
        let films = self.films.borrow();
        let current = self.current.get();
        for (index, film) in films.iter().enumerate() {
            let remove = flat_button("edit-delete-symbolic", "Remove from the Playlist");
            // During a session, the film that plays stays.
            remove.set_sensitive(Some(index) != current || !self.playback.status().running);
            let row = adw::ActionRow::builder()
                .use_markup(false)
                .title(&film.name)
                .subtitle(playlist_row_details(film))
                .tooltip_text(&film.name)
                .title_lines(1)
                .subtitle_lines(1)
                .activatable(true)
                .build();
            let marker = gtk::Image::builder()
                .icon_name(PLAY_ICON)
                .opacity(if Some(index) == current { 1.0 } else { 0.0 })
                .build();
            row.add_prefix(&marker);
            row.add_suffix(&remove);
            let this = Rc::clone(self);
            row.connect_activated(move |_| this.go_to_film(index));

            let this = Rc::clone(self);
            remove.connect_clicked(move |_| this.remove_film(index));
            self.playlist.append(&row);
        }
    }

    fn remove_film(self: &Rc<Self>, index: usize) {
        {
            let mut films = self.films.borrow_mut();
            if index >= films.len() {
                return;
            }
            films.remove(index);
            let count = films.len();
            let current = self.current.get().unwrap_or(0);
            self.current.set(if count == 0 {
                None
            } else if index < current {
                Some(current - 1)
            } else {
                Some(current.min(count - 1))
            });
        }
        self.show_current();
        self.rebuild_playlist();
        self.apply_current_subtitles();
    }

    fn apply_current_subtitles(&self) {
        self.playback
            .set_subtitles(self.current_film().and_then(|film| film.subtitles));
    }

    fn step_film(self: &Rc<Self>, forward: bool) {
        let Some(index) = self.current.get() else {
            return;
        };
        let wanted = if forward {
            index + 1
        } else {
            index.saturating_sub(1)
        };
        if wanted != index {
            self.go_to_film(wanted);
        }
    }

    /// Makes this film the current one, and plays it if a session is running.
    fn go_to_film(self: &Rc<Self>, index: usize) {
        let Some(film) = self.films.borrow().get(index).cloned() else {
            return;
        };
        let status = self.playback.status();
        self.remember_position(status);
        self.current.set(Some(index));
        self.show_current();
        self.rebuild_playlist();
        self.pending_seek.set(None);
        if status.running {
            self.playback.set_paused(false);
            self.start_film(&film);
            self.scale
                .set_range(0.0, film.info.duration_ms as f64 / MILLISECONDS_PER_SECOND);
            self.notice_resume();
        }
    }

    /// Says when a film goes on from where it was left, with a way to start over.
    fn notice_resume(&self) {
        let Some(position) = self.resuming_at.take() else {
            return;
        };
        let toast = adw::Toast::builder()
            .title(format!("Going on from {}", format_time(position)))
            .button_label("Start Over")
            .use_markup(false)
            .build();
        let playback = Arc::clone(&self.playback);
        toast.connect_button_clicked(move |_| playback.seek(0));
        self.toasts.add_toast(toast);
    }

    fn open_subtitles(self: &Rc<Self>, path: &Path) {
        match Subtitles::load(path) {
            Ok(subtitles) => self.set_subtitles(Some(Arc::new(subtitles)), Some(path)),
            Err(err) => {
                error!(error = %err, "cannot use the subtitles");
                self.show_toast(&sentence(&err.to_string()));
            }
        }
    }

    fn set_subtitles(self: &Rc<Self>, subtitles: Option<Arc<Subtitles>>, path: Option<&Path>) {
        let Some(index) = self.current.get() else {
            return;
        };
        if let Some(film) = self.films.borrow_mut().get_mut(index) {
            film.subtitles.clone_from(&subtitles);
            film.subtitle_name = path.map(display_name);
        }
        self.subtitle_row
            .set_subtitle(&path.map_or_else(|| NO_SUBTITLES.to_owned(), display_name));
        self.remove_subtitles.set_sensitive(subtitles.is_some());
        self.playback.set_subtitles(subtitles);
        self.rebuild_playlist();
    }

    fn apply_style(&self) {
        let settings = self.settings.get();
        self.playback.set_subtitle_size(settings.subtitle_size);
        self.playback
            .set_subtitle_position(settings.subtitle_position);
        // The range above makes this conversion lossless.
        self.playback
            .set_subtitle_delay_ms((self.delay.value() * MILLISECONDS_PER_SECOND).round() as i64);
    }

    /// Goes 10 seconds back or forward. Repeating adds up, as the position follows
    /// at once.
    fn step_within_film(&self, forward: bool) {
        let status = self.playback.status();
        if status.running {
            self.playback.seek(stepped_position(status, forward));
        }
    }

    fn play_or_pause(&self) {
        let status = self.playback.status();
        if !status.running {
            return;
        }
        if status.ended {
            self.playback.set_paused(false);
            self.playback.seek(0);
        } else {
            self.playback.set_paused(!status.paused);
        }
    }

    fn remember_position(&self, status: PlaybackStatus) {
        let Some(film) = self.current_film() else {
            return;
        };
        if status.position_ms > 0 {
            let duration = status.duration_ms.max(film.info.duration_ms);
            self.positions
                .borrow_mut()
                .remember(&film.path, status.position_ms, duration);
        }
        self.position_saved.set(Instant::now());
    }

    /// Follows the running film, carries out a seek once a drag has settled, and goes
    /// on to the next film at the end of one.
    fn refresh_controls(self: &Rc<Self>) {
        if let Some((seconds, changed)) = self.pending_seek.get()
            && changed.elapsed() >= SEEK_SETTLE
        {
            self.pending_seek.set(None);
            // The scale's range is the film's length, so this cannot overflow.
            self.playback
                .seek((seconds.max(0.0) * MILLISECONDS_PER_SECOND) as u64);
        }

        let status = self.playback.status();
        self.play.set_sensitive(status.running);
        self.back.set_sensitive(status.running);
        self.forward.set_sensitive(status.running);
        self.scale.set_sensitive(status.running);

        let playing = status.running.then_some(!status.paused && !status.ended);
        self.play.set_icon_name(if playing == Some(true) {
            PAUSE_ICON
        } else {
            PLAY_ICON
        });
        if playing != self.last_playing.get() {
            self.last_playing.set(playing);
            if let Some(on_playing) = self.on_playing.borrow().as_ref() {
                on_playing(playing);
            }
        }
        if status.running && !self.was_running.get() {
            self.notice_resume();
            self.rebuild_playlist();
        }
        self.was_running.set(status.running);
        if !status.running {
            return;
        }

        if self.pending_seek.get().is_none() {
            if status.duration_ms > 0 {
                self.scale
                    .set_range(0.0, status.duration_ms as f64 / MILLISECONDS_PER_SECOND);
            }
            self.scale
                .set_value(status.position_ms as f64 / MILLISECONDS_PER_SECOND);
            self.time
                .set_text(&time_text(status.position_ms, status.duration_ms));
        }
        if self.position_saved.get().elapsed() >= POSITION_SAVE_INTERVAL {
            self.remember_position(status);
        }
        if status.ended {
            self.remember_position(status);
            let next = self.current.get().map_or(0, |index| index + 1);
            if next < self.films.borrow().len() {
                self.go_to_film(next);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn time_is_short_for_short_films() {
        assert_eq!(format_time(0), "0:00");
        assert_eq!(format_time(83_000), "1:23");
        assert_eq!(format_time(3_723_000), "1:02:03");
        assert_eq!(time_text(83_000, 3_723_000), "1:23 / 1:02:03");
    }

    #[test]
    fn a_summary_mentions_more_than_one_sound_track() {
        let info = |tracks: usize| FileInfo {
            duration_ms: 60_000,
            audio_tracks: vec!["x".to_owned(); tracks],
        };
        assert_eq!(file_summary(&info(0)), "1:00 · No sound");
        assert_eq!(file_summary(&info(1)), "1:00");
        assert_eq!(file_summary(&info(3)), "1:00 · 3 sound tracks");
    }

    #[test]
    fn a_summary_says_where_in_the_playlist_a_film_is() {
        let info = FileInfo {
            duration_ms: 60_000,
            audio_tracks: vec!["x".to_owned()],
        };
        assert_eq!(playlist_summary(0, 1, &info), "1:00");
        assert_eq!(playlist_summary(1, 5, &info), "2 of 5 · 1:00");
    }

    #[test]
    fn a_step_stays_within_the_film() {
        let status = |position_ms, duration_ms| PlaybackStatus {
            position_ms,
            duration_ms,
            ..PlaybackStatus::default()
        };
        assert_eq!(stepped_position(status(5000, 60_000), false), 0);
        assert_eq!(stepped_position(status(25_000, 60_000), false), 15_000);
        assert_eq!(stepped_position(status(25_000, 60_000), true), 35_000);
        assert_eq!(stepped_position(status(55_000, 60_000), true), 60_000);
        assert_eq!(stepped_position(status(55_000, 0), true), 65_000);
    }

    #[test]
    fn subtitles_are_found_next_to_a_film() {
        let directory =
            std::env::temp_dir().join(format!("monitorbeam-next-to-{}", std::process::id()));
        std::fs::create_dir_all(&directory).unwrap();
        let film = directory.join("film.mkv");
        assert_eq!(subtitles_next_to(&film), None);
        std::fs::write(directory.join("film.srt"), "x").unwrap();
        assert_eq!(subtitles_next_to(&film), Some(directory.join("film.srt")));
        std::fs::remove_dir_all(directory).unwrap();
    }
}
