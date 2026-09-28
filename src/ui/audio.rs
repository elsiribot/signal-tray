//! Playing voice messages and other audio: one player for the app, and
//! controls in the bubbles.
//!
//! The chat view is rebuilt on every refresh, so the player keeps the
//! state and the controls are just views of it: each render registers
//! its controls by memo, and only the current memo's get updated.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::rc::{Rc, Weak};
use std::time::Duration;

use gstreamer as gst;
use gstreamer_pbutils as gst_pbutils;
use gstreamer_play as gst_play;
use gtk::prelude::*;
use gtk::{gio, glib};

use super::UiEvent;
use crate::model::AccountId;
use crate::store::AttachmentId;

/// How often the progress of the playing memo is updated.
const POSITION_INTERVAL_MS: u32 = 100;
/// Longest wait for a file's duration.
const PROBE_TIMEOUT: gst::ClockTime = gst::ClockTime::from_seconds(5);
const SPEEDS: [f64; 3] = [1.0, 1.5, 2.0];

#[derive(Clone, Debug)]
pub struct AudioView {
    pub id: AttachmentId,
    pub state: AudioState,
    /// Recorded as a voice message, rather than any audio file.
    pub voice: bool,
    pub file_name: Option<String>,
    pub played: bool,
    pub duration: Option<Duration>,
}

#[derive(Clone, Debug)]
pub enum AudioState {
    /// Not downloaded yet.
    Loading,
    Ready(PathBuf),
    /// The download failed.
    Unavailable,
}

/// A memo, in one account.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Key {
    pub account: AccountId,
    pub id: AttachmentId,
}

/// A memo of the open chat, in chat order.
#[derive(Clone, Debug)]
pub struct Queued {
    pub key: Key,
    /// Position of its message in the chat.
    pub message: usize,
    /// `None` until downloaded.
    pub path: Option<PathBuf>,
    pub played: bool,
}

/// What to play after the memo at `current` ends: the next one, if it
/// belongs to the same or the very next message, like Signal does.
pub fn next_in_queue(queue: &[Queued], current: &Key) -> Option<usize> {
    let at = queue.iter().position(|q| &q.key == current)?;
    let next = queue.get(at + 1)?;
    (next.message <= queue[at].message + 1 && next.path.is_some()).then_some(at + 1)
}

/// What `Ctrl+Space` starts in a chat: the first memo not listened to,
/// else the newest.
pub fn pick(queue: &[Queued]) -> Option<usize> {
    let ready = || queue.iter().enumerate().filter(|(_, q)| q.path.is_some());
    ready()
        .find(|(_, q)| !q.played)
        .or_else(|| ready().next_back())
        .map(|(i, _)| i)
}

/// `m:ss`, or `h:mm:ss` for long recordings.
pub fn clock(duration: Duration) -> String {
    let secs = duration.as_secs();
    let (h, m, s) = (secs / 3600, secs / 60 % 60, secs % 60);
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m}:{s:02}")
    }
}

/// From GStreamer's threads to the main loop.
enum Update {
    Position(Option<gst::ClockTime>),
    Duration(Option<gst::ClockTime>),
    State(gst_play::PlayState),
    Ended,
    Error(String),
}

struct Current {
    key: Key,
    playing: bool,
    position: Duration,
    /// A seek requested before playback started.
    pending_seek: Option<Duration>,
}

/// The widgets of one memo's controls.
#[derive(Clone)]
struct Controls {
    button: gtk::Button,
    scale: gtk::Scale,
    time: gtk::Label,
    unplayed: gtk::Label,
    speed: gtk::Button,
}

#[derive(Default)]
struct State {
    current: Option<Current>,
    speed: usize,
    controls: HashMap<Key, Vec<Controls>>,
    queue: Vec<Queued>,
    durations: HashMap<Key, Duration>,
    probing: HashSet<Key>,
}

pub struct Player {
    weak_self: Weak<Self>,
    play: gst_play::Play,
    _signals: gst_play::PlaySignalAdapter,
    state: RefCell<State>,
    events: async_channel::Sender<UiEvent>,
    on_error: Box<dyn Fn(&str)>,
}

impl Player {
    /// `None` if GStreamer can't be initialized; audio then can't be played.
    pub fn new(
        events: async_channel::Sender<UiEvent>,
        on_error: impl Fn(&str) + 'static,
    ) -> Option<Rc<Self>> {
        if let Err(err) = gst::init() {
            tracing::error!(%err, "initializing GStreamer failed; audio can't be played");
            return None;
        }
        let play = gst_play::Play::new(None::<gst_play::PlayVideoRenderer>);
        let mut config = play.config();
        config.set_position_update_interval(POSITION_INTERVAL_MS);
        if let Err(err) = play.set_config(config) {
            tracing::warn!(%err, "configuring the player failed");
        }
        // Keeps the pitch when played faster.
        match gst::ElementFactory::make("scaletempo").build() {
            Ok(scaletempo) => play.pipeline().set_property("audio-filter", &scaletempo),
            Err(err) => tracing::warn!(%err, "no scaletempo; faster playback raises the pitch"),
        }

        let (tx, rx) = async_channel::unbounded();
        let signals = gst_play::PlaySignalAdapter::new(&play);
        let send = |tx: &async_channel::Sender<Update>| {
            let tx = tx.clone();
            move |update| {
                // Fails only once the player is gone.
                let _ = tx.try_send(update);
            }
        };
        let s = send(&tx);
        signals.connect_position_updated(move |_, position| s(Update::Position(position)));
        let s = send(&tx);
        signals.connect_duration_changed(move |_, duration| s(Update::Duration(duration)));
        let s = send(&tx);
        signals.connect_state_changed(move |_, state| s(Update::State(state)));
        let s = send(&tx);
        signals.connect_end_of_stream(move |_| s(Update::Ended));
        let s = send(&tx);
        signals.connect_error(move |_, err, _| s(Update::Error(err.to_string())));

        let player = Rc::new_cyclic(|weak_self| Self {
            weak_self: weak_self.clone(),
            play,
            _signals: signals,
            state: RefCell::default(),
            events,
            on_error: Box::new(on_error),
        });
        let weak = Rc::downgrade(&player);
        glib::spawn_future_local(async move {
            while let Ok(update) = rx.recv().await {
                let Some(player) = weak.upgrade() else { break };
                player.on_update(update);
            }
        });
        Some(player)
    }

    /// Forgets the controls of the previous render.
    pub fn begin_render(&self) {
        self.state.borrow_mut().controls.clear();
    }

    /// The memos of the rendered chat, for playing the next one and for
    /// `Ctrl+Space`.
    pub fn set_queue(&self, queue: Vec<Queued>) {
        self.state.borrow_mut().queue = queue;
    }

    /// Controls for a memo in a bubble.
    pub fn controls(&self, account: &AccountId, view: &AudioView) -> gtk::Widget {
        let key = Key {
            account: account.clone(),
            id: view.id,
        };
        let root = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .css_classes(["audio"])
            .build();
        if !view.voice {
            let name = view.file_name.as_deref().unwrap_or("Audio");
            root.append(&label(&format!("🎵 {name}"), &["audio-name"]));
        }
        let path = match &view.state {
            AudioState::Ready(path) => path.clone(),
            AudioState::Loading => {
                root.append(&label("🎤 Downloading…", &["dim"]));
                return root.upcast();
            }
            AudioState::Unavailable => {
                root.append(&label("🎤 Voice message not available", &["dim"]));
                return root.upcast();
            }
        };

        let button = gtk::Button::builder()
            .icon_name("media-playback-start-symbolic")
            .tooltip_text("Play (Ctrl+Space)")
            .focusable(false)
            .valign(gtk::Align::Center)
            .css_classes(["circular", "play"])
            .build();
        let scale = gtk::Scale::builder()
            .orientation(gtk::Orientation::Horizontal)
            .adjustment(&gtk::Adjustment::new(0.0, 0.0, 1.0, 0.1, 1.0, 0.0))
            .draw_value(false)
            .focusable(false)
            .hexpand(true)
            .width_request(140)
            .build();
        let time = label("", &["meta", "audio-time"]);
        let unplayed = label("●", &["unplayed"]);
        unplayed.set_tooltip_text(Some("Not played yet"));
        let speed = gtk::Button::builder()
            .focusable(false)
            .halign(gtk::Align::End)
            .hexpand(true)
            .tooltip_text("Playback speed")
            .css_classes(["flat", "speed"])
            .build();
        let below = gtk::Box::new(gtk::Orientation::Horizontal, 4);
        below.append(&time);
        below.append(&unplayed);
        below.append(&speed);
        let right = gtk::Box::new(gtk::Orientation::Vertical, 0);
        right.append(&scale);
        right.append(&below);
        let row = gtk::Box::new(gtk::Orientation::Horizontal, 4);
        row.append(&button);
        row.append(&right);
        root.append(&row);

        let weak = self.weak_self.clone();
        let (k, p) = (key.clone(), path.clone());
        button.connect_clicked(move |_| {
            if let Some(player) = weak.upgrade() {
                player.toggle(&k, &p);
            }
        });
        let weak = self.weak_self.clone();
        let (k, p) = (key.clone(), path.clone());
        scale.connect_change_value(move |_, _, value| {
            if let Some(player) = weak.upgrade() {
                player.seek(&k, &p, Duration::from_secs_f64(value.max(0.0)));
            }
            glib::Propagation::Proceed
        });
        let weak = self.weak_self.clone();
        speed.connect_clicked(move |_| {
            if let Some(player) = weak.upgrade() {
                player.cycle_speed();
            }
        });

        let controls = Controls {
            button,
            scale,
            time,
            unplayed,
            speed,
        };
        {
            let mut state = self.state.borrow_mut();
            if let Some(duration) = view.duration {
                state.durations.insert(key.clone(), duration);
            }
            state
                .controls
                .entry(key.clone())
                .or_default()
                .push(controls.clone());
        }
        self.show(&key, &controls, view.played);
        if view.duration.is_none() {
            self.probe(key, path);
        }
        root.upcast()
    }

    /// Pauses or resumes what plays in the rendered chat, or starts its
    /// first unplayed memo (else the newest).
    pub fn toggle_in_chat(&self) {
        let target = {
            let state = self.state.borrow();
            let current = state
                .current
                .as_ref()
                .filter(|c| state.queue.iter().any(|q| q.key == c.key))
                .map(|c| c.key.clone());
            current
                .and_then(|key| state.queue.iter().find(|q| q.key == key))
                .or_else(|| pick(&state.queue).map(|i| &state.queue[i]))
                .and_then(|q| Some((q.key.clone(), q.path.clone()?)))
        };
        if let Some((key, path)) = target {
            self.toggle(&key, &path);
        }
    }

    fn toggle(&self, key: &Key, path: &Path) {
        let playing = {
            let state = self.state.borrow();
            state
                .current
                .as_ref()
                .filter(|c| &c.key == key)
                .map(|c| c.playing)
        };
        match playing {
            Some(true) => self.play.pause(),
            Some(false) => self.play.play(),
            None => self.start(key, path, None),
        }
    }

    fn seek(&self, key: &Key, path: &Path, position: Duration) {
        let is_current = self
            .state
            .borrow()
            .current
            .as_ref()
            .is_some_and(|c| &c.key == key);
        if is_current {
            self.play.seek(clock_time(position));
        } else {
            self.start(key, path, Some(position));
        }
    }

    fn start(&self, key: &Key, path: &Path, seek: Option<Duration>) {
        let previous = {
            let mut state = self.state.borrow_mut();
            let previous = state.current.replace(Current {
                key: key.clone(),
                playing: false,
                position: Duration::ZERO,
                pending_seek: seek,
            });
            previous.map(|p| p.key)
        };
        self.play.stop();
        self.play
            .set_uri(Some(gio::File::for_path(path).uri().as_str()));
        self.play.set_rate(self.rate());
        self.play.play();
        if let Some(previous) = previous {
            self.refresh(&previous);
        }
        self.mark_played(key);
        self.refresh(key);
    }

    fn mark_played(&self, key: &Key) {
        let newly = {
            let mut state = self.state.borrow_mut();
            match state.queue.iter_mut().find(|q| &q.key == key) {
                Some(queued) if !queued.played => {
                    queued.played = true;
                    true
                }
                _ => false,
            }
        };
        if newly {
            let _ = self.events.try_send(UiEvent::Played {
                account: key.account.clone(),
                id: key.id,
            });
        }
    }

    fn cycle_speed(&self) {
        {
            let mut state = self.state.borrow_mut();
            state.speed = (state.speed + 1) % SPEEDS.len();
        }
        self.play.set_rate(self.rate());
        if let Some(key) = self.current_key() {
            self.refresh(&key);
        }
    }

    fn rate(&self) -> f64 {
        SPEEDS[self.state.borrow().speed]
    }

    fn current_key(&self) -> Option<Key> {
        self.state.borrow().current.as_ref().map(|c| c.key.clone())
    }

    fn on_update(&self, update: Update) {
        let Some(key) = self.current_key() else {
            return;
        };
        match update {
            Update::Position(position) => {
                if let Some(current) = self.state.borrow_mut().current.as_mut() {
                    current.position = position.map(Duration::from).unwrap_or_default();
                }
            }
            Update::Duration(Some(duration)) => {
                let duration = Duration::from(duration);
                let known = self
                    .state
                    .borrow_mut()
                    .durations
                    .insert(key.clone(), duration);
                if known.is_none() {
                    self.duration_known(&key, duration);
                }
            }
            Update::Duration(None) => {}
            Update::State(state) => {
                let playing = state == gst_play::PlayState::Playing;
                let seek = match self.state.borrow_mut().current.as_mut() {
                    Some(current) => {
                        current.playing = playing;
                        current.pending_seek.take().filter(|_| playing)
                    }
                    None => None,
                };
                if let Some(position) = seek {
                    self.play.seek(clock_time(position));
                }
            }
            Update::Ended => {
                self.play.stop();
                self.state.borrow_mut().current = None;
                self.refresh(&key);
                let next = {
                    let state = self.state.borrow();
                    next_in_queue(&state.queue, &key).and_then(|i| {
                        Some((state.queue[i].key.clone(), state.queue[i].path.clone()?))
                    })
                };
                if let Some((next, path)) = next {
                    self.start(&next, &path, None);
                }
                return;
            }
            Update::Error(err) => {
                tracing::warn!(%err, "playing audio failed");
                self.play.stop();
                self.state.borrow_mut().current = None;
                (self.on_error)(&format!("Can't play this: {err}"));
            }
        }
        self.refresh(&key);
    }

    /// Updates every control showing `key`.
    fn refresh(&self, key: &Key) {
        let controls = self
            .state
            .borrow()
            .controls
            .get(key)
            .cloned()
            .unwrap_or_default();
        let played = self
            .state
            .borrow()
            .queue
            .iter()
            .find(|q| &q.key == key)
            .is_none_or(|q| q.played);
        for controls in &controls {
            self.show(key, controls, played);
        }
    }

    fn show(&self, key: &Key, controls: &Controls, played: bool) {
        let state = self.state.borrow();
        let duration = state.durations.get(key).copied();
        let current = state.current.as_ref().filter(|c| &c.key == key);
        let playing = current.is_some_and(|c| c.playing);
        controls.button.set_icon_name(if playing {
            "media-playback-pause-symbolic"
        } else {
            "media-playback-start-symbolic"
        });
        controls.button.set_tooltip_text(Some(if playing {
            "Pause (Ctrl+Space)"
        } else {
            "Play (Ctrl+Space)"
        }));
        let position = current.map(|c| c.position).unwrap_or_default();
        let adjustment = controls.scale.adjustment();
        adjustment.set_upper(duration.map_or(1.0, |d| d.as_secs_f64().max(0.1)));
        adjustment.set_value(position.as_secs_f64());
        // Rounded, so a 1.8 s memo isn't announced as 0:01. (`clock` cuts.)
        let total = duration
            .map(|d| clock(d + Duration::from_millis(500)))
            .unwrap_or_else(|| "–:––".to_owned());
        controls.time.set_label(&match current {
            Some(_) => format!("{} / {total}", clock(position)),
            None => total,
        });
        controls.unplayed.set_visible(!played && current.is_none());
        controls.speed.set_visible(current.is_some());
        controls
            .speed
            .set_label(&format!("{}×", SPEEDS[state.speed]));
    }

    /// Finds out a memo's duration in the background.
    fn probe(&self, key: Key, path: PathBuf) {
        {
            let mut state = self.state.borrow_mut();
            if state.durations.contains_key(&key) || !state.probing.insert(key.clone()) {
                return;
            }
        }
        let weak = self.weak_self.clone();
        glib::spawn_future_local(async move {
            let uri = gio::File::for_path(&path).uri().to_string();
            let probed = gio::spawn_blocking(move || {
                let discoverer = gst_pbutils::Discoverer::new(PROBE_TIMEOUT)?;
                Ok::<_, glib::Error>(discoverer.discover_uri(&uri)?.duration())
            })
            .await;
            let Some(player) = weak.upgrade() else { return };
            player.state.borrow_mut().probing.remove(&key);
            match probed {
                Ok(Ok(Some(duration))) => {
                    let duration = Duration::from(duration);
                    player
                        .state
                        .borrow_mut()
                        .durations
                        .insert(key.clone(), duration);
                    player.duration_known(&key, duration);
                    player.refresh(&key);
                }
                Ok(Ok(None)) => {}
                Ok(Err(err)) => tracing::info!(%err, path = %path.display(), "can't read audio"),
                Err(_) => {}
            }
        });
    }

    fn duration_known(&self, key: &Key, duration: Duration) {
        let _ = self.events.try_send(UiEvent::AudioDuration {
            account: key.account.clone(),
            id: key.id,
            duration,
        });
    }
}

fn clock_time(duration: Duration) -> gst::ClockTime {
    gst::ClockTime::from_nseconds(u64::try_from(duration.as_nanos()).unwrap_or(u64::MAX))
}

fn label(text: &str, classes: &[&str]) -> gtk::Label {
    gtk::Label::builder()
        .label(text)
        .xalign(0.0)
        .css_classes(classes.to_vec())
        .build()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn queued(id: i64, message: usize, played: bool, ready: bool) -> Queued {
        Queued {
            key: Key {
                account: AccountId::try_from("1".to_owned()).expect("valid account id"),
                id: AttachmentId(id),
            },
            message,
            path: ready.then(|| PathBuf::from(format!("{id}.aac"))),
            played,
        }
    }

    #[test]
    fn the_next_memo_plays_only_if_it_follows_directly() {
        let queue = [
            queued(1, 0, true, true),
            queued(2, 1, false, true),
            queued(3, 1, false, true),
            queued(4, 5, false, true),
            queued(5, 6, false, false),
        ];
        let next = |id: usize| next_in_queue(&queue, &queue[id - 1].key).map(|i| i + 1);
        assert_eq!(next(1), Some(2), "the very next message");
        assert_eq!(next(2), Some(3), "the same message");
        assert_eq!(next(3), None, "messages in between");
        assert_eq!(next(4), None, "not downloaded");
        assert_eq!(next(5), None, "the last");
    }

    #[test]
    fn ctrl_space_starts_the_first_unplayed_memo_else_the_newest() {
        let queue = [
            queued(1, 0, true, true),
            queued(2, 1, false, false),
            queued(3, 2, false, true),
            queued(4, 3, false, true),
        ];
        assert_eq!(pick(&queue), Some(2), "skips what isn't downloaded");
        let all_played = [queued(1, 0, true, true), queued(2, 1, true, true)];
        assert_eq!(pick(&all_played), Some(1));
        assert_eq!(pick(&[]), None);
    }

    #[test]
    fn clock_format() {
        assert_eq!(clock(Duration::from_millis(4_900)), "0:04");
        assert_eq!(clock(Duration::from_secs(75)), "1:15");
        assert_eq!(clock(Duration::from_secs(3_725)), "1:02:05");
    }
}
