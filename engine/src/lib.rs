//! `bo-engine` — the actual execution on top of bo's data model.
//!
//! The model in [`bo_core`] says *what* plays; this crate makes sound from
//! it. Flattened at the crate root: the transport ([`Player`]) over a set of
//! stacked tracks, plus the [`Backend`] seam through which sound actually
//! comes out.
//!
//! The player owns three things and nothing else: the tracks, a playhead
//! timecode, and a state (`Stopped`/`Playing`/`Paused`). Which clips are audible
//! at a given timecode is *derived* (`active_clips`) rather than stored, so the
//! mix can never drift out of sync with the arrangement.

use std::fmt;
use std::sync::Arc;
use std::time::Duration;

pub mod measure;
pub mod rodio;
pub mod timeline;

use bo_core::bus::{Bus, Group};
use bo_core::command::{Command, Error, Outcome, Overlap, PlacedClip, Put};
use bo_core::track::{Clip, Fade, Source, Track};

/// Why a backend could not do what it was told: data, shared with the
/// command vocabulary ([`bo_core::command`]).
pub use bo_core::command::BackendError;

/// How an edit reached the sound: data, shared with the command vocabulary
/// ([`bo_core::command`]).
pub use bo_core::command::Landed;

/// One arrangement edit, addressed the way the CLI addresses it. This is what
/// a running graph is asked to take without being rebuilt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Change {
    /// A track's gain changed: its volume, or its mute.
    TrackGain(usize),
    /// A track's placement (pan) changed.
    TrackPan(usize),
    /// A clip's gain or fade envelope changed.
    ClipParams(usize, u64),
    /// A clip's own placement changed (set, or given back to its track).
    ClipPan(usize, u64),
    /// A clip's control sources changed — a curve edited, plugged or
    /// unplugged. A running chain reads them through shared state, so the
    /// edit is a store, not a rebuild.
    ClipControls(usize, u64),
    /// The same, for the clip's gain input.
    ClipGainControls(usize, u64),
    /// Clips were placed past the end of a track's queued material.
    Appended(usize),
    /// The arrangement's shape changed: a clip was removed or moved, or the
    /// whole arrangement was replaced. Nothing running can express that.
    Structure,
    /// A group bus's gain or mute changed. A running graph cannot take it —
    /// the group strip is baked when the graph is built — so it waits for an
    /// `apply`, which rebuilds.
    GroupGain(u64),
}

/// What [`Player::apply`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Applied {
    /// Nothing was waiting.
    Nothing,
    /// `n` edits landed on the running graph; it was not rebuilt.
    Live(usize),
    /// `live` edits landed and the rest needed a graph rebuilt from `at`.
    Rebuilt { live: usize, at: Duration },
    /// The transport is stopped: there is no graph, so every edit stays
    /// pending until it plays.
    NotPlaying,
}

/// The thing that makes sound.
///
/// `Player` never decodes anything itself; it decides *what* should be audible
/// and hands that over. Implementations: [`Silent`] here (headless, used by
/// tests and CI), and whatever real audio backend the daemon picks at startup.
pub trait Backend {
    /// Start (or restart) playback of `tracks` from playhead `at`.
    ///
    /// `at` is a track timecode; an implementation reads each clip from
    /// `clip.from + (at - clip.at)` and skips clips that have already passed.
    /// `groups` is the bus table: the strips tracks routed to a group bus feed
    /// through, read when a graph is built exactly like the tracks themselves.
    fn play(
        &mut self,
        tracks: &[Track],
        groups: &[Group],
        at: Duration,
    ) -> Result<(), BackendError>;

    /// Suspend output, keeping position.
    fn pause(&mut self);

    /// Resume after [`Backend::pause`], without re-planning.
    fn resume(&mut self);

    /// Silence everything and forget position.
    fn stop(&mut self);

    /// Master gain, already clamped to `0.0 ..= 1.0`.
    fn set_volume(&mut self, volume: f32);

    /// Take one arrangement edit on a graph that is already running, without
    /// interrupting it. `false` means this backend cannot express the edit
    /// live, so the caller keeps it pending and re-plans at the next `apply`.
    ///
    /// `tracks` is the arrangement as edited (the values to land), and `at`
    /// is where the graph is sounding now, so a backend can tell a clip that
    /// already passed from one still to come.
    fn land(&mut self, tracks: &[Track], at: Duration, change: &Change) -> bool {
        let _ = (tracks, at, change);
        false
    }

    /// Where the audio clock really is, if this backend can say. Used to
    /// correct the playhead against drift.
    fn position(&self) -> Option<Duration> {
        None
    }
}

/// A backend that hears nothing: for tests, CI, and `--backend silent`.
#[derive(Debug, Default, Clone)]
pub struct Silent {
    /// Last `play` request, as `(track count, start timecode)`.
    pub last_play: Option<(usize, Duration)>,
    /// Group count in the last `play` request; lets a test assert the bus
    /// table travels with the tracks.
    pub last_groups: Option<usize>,
    /// Requests so far, in order. Lets a test assert "seek re-planned".
    pub events: Vec<BackendEvent>,
    /// The edits offered to [`Backend::land`], in order.
    pub landed: Vec<Change>,
    volume: f32,
}

/// One thing a [`Silent`] backend was asked to do.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum BackendEvent {
    Play,
    Pause,
    Resume,
    Stop,
    SetVolume,
    Land,
}

impl Backend for Silent {
    fn play(
        &mut self,
        tracks: &[Track],
        groups: &[Group],
        at: Duration,
    ) -> Result<(), BackendError> {
        self.last_play = Some((tracks.len(), at));
        self.last_groups = Some(groups.len());
        self.events.push(BackendEvent::Play);
        Ok(())
    }

    fn pause(&mut self) {
        self.events.push(BackendEvent::Pause);
    }

    fn resume(&mut self) {
        self.events.push(BackendEvent::Resume);
    }

    fn stop(&mut self) {
        self.events.push(BackendEvent::Stop);
    }

    fn set_volume(&mut self, volume: f32) {
        self.volume = volume;
        self.events.push(BackendEvent::SetVolume);
    }

    /// Takes every edit a real graph could take live — gains, fades, clips
    /// appended to a tail — and refuses the ones that need a graph built
    /// from scratch, so tests see the same split the daemon does.
    fn land(&mut self, _tracks: &[Track], _at: Duration, change: &Change) -> bool {
        self.events.push(BackendEvent::Land);
        self.landed.push(change.clone());
        !matches!(change, Change::Structure | Change::GroupGain(_))
    }
}

impl Silent {
    /// Volume last requested, for assertions.
    #[must_use]
    pub fn volume(&self) -> f32 {
        self.volume
    }
}

/// Transport state.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum State {
    /// Nothing loaded into the backend; playhead is at zero.
    #[default]
    Stopped,
    Playing,
    Paused,
}

impl fmt::Display for State {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Stopped => "stopped",
            Self::Playing => "playing",
            Self::Paused => "paused",
        })
    }
}

/// A playhead over a set of simultaneously mixed tracks, and the buses their
/// outputs feed — the master and any group buses.
#[derive(Debug)]
pub struct Player<B: Backend = Silent> {
    tracks: Vec<Track>,
    playhead: Duration,
    state: State,
    bus: Bus,
    /// The group buses (shown to users as plain buses): summing points
    /// several tracks' outputs can be routed into, each carrying its own
    /// strip before its sum feeds the master.
    groups: Vec<Group>,
    /// Next id for a group; ids are never reused while the group lives.
    next_group_id: u64,
    backend: B,
    /// Arrangement edits the running graph could not take, waiting for the
    /// next `apply`, `play` or `resume`.
    pending: Vec<Change>,
}

impl Default for Player<Silent> {
    fn default() -> Self {
        Self::new(Silent::default())
    }
}

impl<B: Backend> Player<B> {
    /// A stopped player over an empty set of tracks and a master bus.
    pub fn new(backend: B) -> Self {
        Self {
            tracks: Vec::new(),
            playhead: Duration::ZERO,
            state: State::Stopped,
            bus: Bus::master(),
            groups: Vec::new(),
            next_group_id: 0,
            backend,
            pending: Vec::new(),
        }
    }

    /// The arrangement being played.
    #[must_use]
    pub fn tracks(&self) -> &[Track] {
        &self.tracks
    }

    /// Edit the arrangement. The player cannot see an edit made through here:
    /// report it with [`Player::changed`], so it lands on the running graph
    /// (or is remembered until a graph can take it).
    pub fn tracks_mut(&mut self) -> &mut Vec<Track> {
        &mut self.tracks
    }

    /// Add a track on top of the existing ones.
    pub fn add_track(&mut self, track: Track) -> usize {
        self.tracks.push(track);
        self.tracks.len() - 1
    }

    /// Remove a track by index.
    pub fn remove_track(&mut self, index: usize) -> Option<Track> {
        (index < self.tracks.len()).then(|| self.tracks.remove(index))
    }

    /// Current transport state.
    #[must_use]
    pub fn state(&self) -> State {
        self.state
    }

    /// Whether the transport is running.
    #[must_use]
    pub fn is_playing(&self) -> bool {
        self.state == State::Playing
    }

    /// The playhead timecode. While running, a backend that knows its own clock
    /// is believed over our own bookkeeping.
    #[must_use]
    pub fn playhead(&self) -> Duration {
        if self.state == State::Playing
            && let Some(real) = self.backend.position()
        {
            return real;
        }
        self.playhead
    }

    /// The master bus: where every track's output lands.
    #[must_use]
    pub fn bus(&self) -> &Bus {
        &self.bus
    }

    /// Master gain, clamped to `0.0 ..= 1.0`.
    #[must_use]
    pub fn volume(&self) -> f32 {
        self.bus.gain()
    }

    /// Set master gain.
    pub fn set_volume(&mut self, volume: f32) {
        let v = volume.clamp(0.0, 1.0);
        self.bus.set_gain(v);
        self.backend.set_volume(v);
    }

    /// The whole arrangement's length: the latest end across tracks.
    #[must_use]
    pub fn duration(&self) -> Duration {
        self.tracks
            .iter()
            .fold(Duration::ZERO, |acc, track| acc.max(track.duration()))
    }

    /// Every clip audible at track time `t`, one per track at most. This *is*
    /// the mix; nothing about it is cached.
    pub fn active_clips(&self, t: Duration) -> impl Iterator<Item = (usize, &Clip)> {
        self.tracks
            .iter()
            .enumerate()
            .filter_map(move |(index, track)| track.clip_at(t).map(|clip| (index, clip)))
    }

    /// Borrow the backend, e.g. to query it directly.
    #[must_use]
    pub fn backend(&self) -> &B {
        &self.backend
    }

    /// Mutable access to the backend.
    pub fn backend_mut(&mut self) -> &mut B {
        &mut self.backend
    }

    /// Replace the backend, e.g. swap in a real audio implementation. Leaves
    /// transport stopped.
    pub fn with_backend<C: Backend>(self, backend: C) -> Player<C> {
        let Player {
            tracks,
            playhead,
            bus,
            groups,
            next_group_id,
            pending,
            ..
        } = self;
        Player {
            tracks,
            playhead,
            state: State::Stopped,
            bus,
            groups,
            next_group_id,
            backend,
            pending,
        }
    }

    /// The edits waiting for a graph that can take them.
    #[must_use]
    pub fn pending(&self) -> &[Change] {
        &self.pending
    }

    /// The group buses: summing points several tracks' outputs can be routed
    /// into, each carrying its own strip before its sum feeds the master.
    #[must_use]
    pub fn groups(&self) -> &[Group] {
        &self.groups
    }

    /// The group bus with `id`, if it exists.
    #[must_use]
    pub fn group(&self, id: u64) -> Option<&Group> {
        self.groups.iter().find(|g| g.id() == id)
    }

    /// The group bus with `id`, mutably, if it exists.
    #[must_use]
    pub fn group_mut(&mut self, id: u64) -> Option<&mut Group> {
        self.groups.iter_mut().find(|g| g.id() == id)
    }

    /// Add a group bus and return its stable id — the handle
    /// [`bo_core::bus::BusRef`]'s `Group` variant addresses it by. Names are
    /// labels, not identity, so nothing checks them here.
    pub fn add_group(&mut self, name: Option<String>) -> u64 {
        let id = self.next_group_id;
        self.next_group_id += 1;
        let mut group = Group::new(id);
        if let Some(name) = name {
            group.set_name(name);
        }
        self.groups.push(group);
        id
    }

    /// Replace the whole group table, e.g. committing a loaded arrangement.
    /// The id counter is advanced past the tallest id, so a later
    /// [`Player::add_group`] can never collide with a loaded group.
    pub fn set_groups(&mut self, groups: Vec<Group>) {
        let next = groups.iter().map(Group::id).max().map_or(0, |id| id + 1);
        self.next_group_id = self.next_group_id.max(next);
        self.groups = groups;
    }
}

impl<B: Backend> Player<B> {
    /// Start playing from the current playhead. A fresh graph takes every
    /// edit, so nothing stays pending.
    pub fn play(&mut self) -> Result<(), BackendError> {
        let at = self.playhead();
        self.backend.play(&self.tracks, &self.groups, at)?;
        self.playhead = at;
        self.pending.clear();
        self.state = State::Playing;
        Ok(())
    }

    /// Hold position and silence output.
    pub fn pause(&mut self) {
        if self.state == State::Playing {
            // Take the audio clock's reading before freezing it, so what we
            // report — and where a later re-plan enters — is really where the
            // sound stopped.
            self.playhead = self.playhead();
            self.backend.pause();
            self.state = State::Paused;
        }
    }

    /// Continue from where `pause` left off.
    ///
    /// Edits made while paused usually land on the paused graph as they are
    /// made; one that could not is given a graph here, rather than being
    /// silently dropped on the floor.
    pub fn resume(&mut self) -> Result<(), BackendError> {
        match self.state {
            State::Paused => {
                if self.pending.is_empty() {
                    self.backend.resume();
                } else {
                    let at = self.playhead();
                    self.backend.play(&self.tracks, &self.groups, at)?;
                    self.playhead = at;
                    self.pending.clear();
                    // A graph built while paused comes out paused; this one
                    // is meant to be running.
                    self.backend.resume();
                }
                self.state = State::Playing;
            }
            State::Stopped => return self.play(),
            State::Playing => {}
        }
        Ok(())
    }

    /// Stop and rewind to zero.
    pub fn stop(&mut self) {
        self.backend.stop();
        self.playhead = Duration::ZERO;
        self.state = State::Stopped;
    }

    /// Drop every track, clear every group bus, and reset the transport:
    /// back to a fresh session. Group ids restart from zero, like clip ids
    /// after a track clears.
    pub fn reset(&mut self) {
        self.stop();
        self.tracks.clear();
        self.groups.clear();
        self.next_group_id = 0;
        self.pending.clear();
    }

    /// Jump the playhead. A running transport is re-planned from the new
    /// position, because most backends cannot seek mid-stream; the fresh
    /// graph takes every pending edit with it.
    pub fn seek(&mut self, at: Duration) -> Result<(), BackendError> {
        self.playhead = at;
        if self.state == State::Playing {
            self.backend.play(&self.tracks, &self.groups, at)?;
            self.pending.clear();
        }
        Ok(())
    }

    /// Report an arrangement edit, so it can take effect.
    ///
    /// The edit is already in the tracks; this is the player being told about
    /// it. A live landing is asked for first, and only what the running graph
    /// cannot express stays pending for the next [`Player::apply`]. A stopped
    /// transport has no graph at all, so everything is pending there — and
    /// lands at the next `play`.
    pub fn changed(&mut self, change: Change) -> Landed {
        if self.state != State::Stopped {
            let at = self.playhead();
            if self.backend.land(&self.tracks, at, &change) {
                return Landed::Live;
            }
        }
        self.remember(change)
    }

    /// Keep an edit for the next graph. A structural edit supersedes
    /// everything waiting: a rebuilt graph takes all of it anyway.
    fn remember(&mut self, change: Change) -> Landed {
        if change == Change::Structure {
            self.pending.clear();
        } else if self.pending.contains(&Change::Structure) {
            return Landed::Pending;
        }
        if !self.pending.contains(&change) {
            self.pending.push(change);
        }
        Landed::Pending
    }

    /// Make every pending edit audible.
    ///
    /// Each edit is offered to the running graph again; only what it still
    /// cannot take forces a rebuild, and a rebuild re-plans from where the
    /// audio really is, so it resumes what the listener is hearing rather
    /// than jumping ahead of it. A stopped transport is left alone: its edits
    /// are pending by definition and land at the next `play`.
    pub fn apply(&mut self) -> Result<Applied, BackendError> {
        if self.state == State::Stopped {
            return Ok(Applied::NotPlaying);
        }
        let at = self.playhead();
        let waiting = std::mem::take(&mut self.pending);
        let total = waiting.len();
        let mut rest = Vec::new();
        for change in waiting {
            if !self.backend.land(&self.tracks, at, &change) {
                rest.push(change);
            }
        }
        let live = total - rest.len();
        if rest.is_empty() {
            return Ok(if live == 0 {
                Applied::Nothing
            } else {
                Applied::Live(live)
            });
        }
        // Something needs a graph of its own. Read the clock again: landing
        // the rest may have taken a moment, and the entry point should be
        // where the sound is now.
        let at = self.playhead();
        if let Err(e) = self.backend.play(&self.tracks, &self.groups, at) {
            // The rebuild did not happen, so those edits are still waiting;
            // what landed live stays landed.
            self.pending = rest;
            return Err(e);
        }
        self.playhead = at;
        Ok(Applied::Rebuilt { live, at })
    }

    /// Move the playhead to match a backend-reported clock, without re-planning.
    pub fn set_playhead(&mut self, at: Duration) {
        self.playhead = at;
    }

    /// Step the playhead forward by wall-clock elapsed time. Backends that
    /// cannot report a position use this.
    pub fn advance(&mut self, dt: Duration) {
        if self.state == State::Playing {
            self.playhead += dt;
        }
    }

    /// Whether playback of the arrangement has run to its end.
    ///
    /// The playhead is compared against the arrangement's known length.
    /// Long-running callers use this to decide when playback is done.
    #[must_use]
    pub fn is_finished(&self) -> bool {
        self.playhead() >= self.duration()
    }
}

/// Execute one [`Command`] against a player-owned arrangement: the engine's
/// single mouth. Every executor runs commands through here — the daemon
/// after decoding them off its wire, a process session directly. A refused
/// command (an unmeasurable source, a collision) is an [`Error`] and leaves
/// the arrangement exactly as it was.
pub fn exec<B: Backend>(player: &mut Player<B>, command: Command) -> Result<Outcome, Error> {
    match command {
        Command::Put { uri, slice, on } => {
            // An open slice plays to the source's end; resolve that end now,
            // so every clip has a known finite length and none can silently
            // block its track. Same refusal the CLI makes.
            let to = match slice.to {
                Some(to) => to,
                None => rodio::probe(&uri).map_err(|why| Error::Probe {
                    uri: uri.clone(),
                    why,
                })?,
            };
            // The track is addressed by index, created on demand like the
            // CLI's.
            while player.tracks().len() <= on.track {
                player.add_track(Track::new());
            }
            let clip = Clip::sliced(Arc::new(Source::new(&uri)), slice.from, to)
                .at(on.at)
                .gain(1.0)
                .fade(Fade::default());
            // Refuse a collision before inserting anything: a rejected put
            // leaves no trace, and says where the clip could go instead.
            let view = &player.tracks()[on.track];
            if let Some(conflict) = view.clips().iter().find(|c| c.overlaps(&clip)) {
                return Err(Error::Overlap(Overlap {
                    track: on.track,
                    at: clip.at,
                    conflict: conflict.id,
                    conflict_at: conflict.at,
                    conflict_end: conflict.end(),
                    next_free: view.next_free_start(clip.at, clip.duration()),
                }));
            }
            let id = player.tracks_mut()[on.track]
                .insert(clip)
                .expect("pre-checked: the insert cannot collide");
            // Placement past the queued tail joins the running graph; the
            // rest waits for an apply — exactly what the daemon does.
            let landed = player.changed(Change::Appended(on.track));
            Ok(Outcome::Put(Put {
                track: on.track,
                clip: PlacedClip {
                    id,
                    uri: uri.clone(),
                    at: on.at,
                    from: slice.from,
                    to,
                    gain: 1.0,
                    fade: Fade::default(),
                },
                landed,
            }))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bo_core::track::Clip;
    use std::sync::Arc;

    fn secs(s: u64) -> Duration {
        Duration::from_secs(s)
    }

    fn track_with(uri: &str, len: u64) -> Track {
        let mut t = Track::named(uri);
        t.insert(Clip::new(
            Arc::new(bo_core::track::Source { uri: uri.into() }),
            secs(len),
        ))
        .unwrap();
        t
    }

    #[test]
    fn transport_state_machine() {
        let mut p: Player<Silent> = Player::default();
        p.add_track(track_with("a.wav", 10));
        assert_eq!(p.state(), State::Stopped);
        assert_eq!(p.playhead(), Duration::ZERO);

        p.play().unwrap();
        assert_eq!(p.state(), State::Playing);
        assert_eq!(p.backend().last_play, Some((1, Duration::ZERO)));

        p.pause();
        assert_eq!(p.state(), State::Paused);
        p.resume().unwrap();
        assert_eq!(p.state(), State::Playing);
        assert_eq!(
            p.backend().events,
            vec![
                BackendEvent::Play,
                BackendEvent::Pause,
                BackendEvent::Resume
            ]
        );

        p.stop();
        assert_eq!(p.state(), State::Stopped);
        assert_eq!(p.playhead(), Duration::ZERO);
        // pause/advance on a stopped transport are no-ops.
        p.pause();
        p.advance(secs(5));
        assert_eq!(p.playhead(), Duration::ZERO);
        assert_eq!(p.state(), State::Stopped);
    }

    #[test]
    fn seeking_replans_a_running_transport_only() {
        let mut p: Player<Silent> = Player::default();
        p.add_track(track_with("a.wav", 10));
        p.seek(secs(4)).unwrap();
        assert_eq!(p.playhead(), secs(4));
        assert_eq!(p.backend().last_play, None, "stopped: nothing to plan");

        p.play().unwrap();
        assert_eq!(
            p.backend().last_play,
            Some((1, secs(4))),
            "starts where we parked"
        );
        p.seek(secs(7)).unwrap();
        assert_eq!(p.backend().last_play, Some((1, secs(7))));
        assert_eq!(
            p.backend()
                .events
                .iter()
                .filter(|e| **e == BackendEvent::Play)
                .count(),
            2
        );
    }

    #[test]
    fn apply_with_nothing_pending_rebuilds_nothing() {
        let mut p: Player<Silent> = Player::default();
        p.add_track(track_with("a.wav", 10));
        let plays = |p: &Player<Silent>| {
            p.backend()
                .events
                .iter()
                .filter(|e| **e == BackendEvent::Play)
                .count()
        };

        assert_eq!(p.apply().unwrap(), Applied::NotPlaying);
        assert_eq!(plays(&p), 0, "stopped: nothing to plan");
        assert_eq!(p.backend().last_play, None);

        p.play().unwrap();
        p.seek(Duration::from_secs(4)).unwrap();
        assert_eq!(plays(&p), 2);
        assert_eq!(p.apply().unwrap(), Applied::Nothing);
        assert_eq!(
            plays(&p),
            2,
            "an apply with nothing waiting does not touch the graph"
        );
        assert_eq!(p.playhead(), Duration::from_secs(4), "nor the playhead");
    }

    #[test]
    fn gains_land_live_and_only_structure_rebuilds() {
        let mut p: Player<Silent> = Player::default();
        p.add_track(track_with("a.wav", 10));
        let plays = |p: &Player<Silent>| {
            p.backend()
                .events
                .iter()
                .filter(|e| **e == BackendEvent::Play)
                .count()
        };
        p.play().unwrap();
        assert_eq!(plays(&p), 1);

        // A gain is something a running graph can take as it is.
        assert_eq!(p.changed(Change::TrackGain(0)), Landed::Live);
        assert_eq!(p.changed(Change::ClipParams(0, 3)), Landed::Live);
        assert_eq!(plays(&p), 1, "no rebuild to change a gain");
        assert!(p.pending().is_empty(), "and nothing left waiting");
        assert_eq!(p.apply().unwrap(), Applied::Nothing);

        // A clip removed or moved is not: it waits, and apply is what gives
        // it a graph.
        assert_eq!(p.changed(Change::Structure), Landed::Pending);
        assert_eq!(p.pending(), &[Change::Structure]);
        assert_eq!(
            p.apply().unwrap(),
            Applied::Rebuilt {
                live: 0,
                at: Duration::ZERO
            }
        );
        assert_eq!(plays(&p), 2, "the rebuild is one re-plan");
        assert!(p.pending().is_empty(), "and it drained the queue");

        // A structural edit supersedes the small ones waiting behind it: one
        // rebuild takes all of it.
        p.changed(Change::TrackGain(0));
        p.backend_mut().landed.clear();
        p.changed(Change::Structure);
        assert_eq!(p.pending(), &[Change::Structure]);
        assert_eq!(p.apply().unwrap(), Applied::Rebuilt { live: 0, at: Duration::ZERO });
        assert_eq!(plays(&p), 3);
    }

    #[test]
    fn edits_made_while_stopped_land_at_the_next_play() {
        let mut p: Player<Silent> = Player::default();
        p.add_track(track_with("a.wav", 10));
        assert_eq!(p.changed(Change::TrackGain(0)), Landed::Pending);
        assert_eq!(p.changed(Change::Structure), Landed::Pending);
        assert_eq!(p.apply().unwrap(), Applied::NotPlaying);
        assert!(
            p.backend().landed.is_empty(),
            "a stopped transport has no graph to offer an edit to"
        );

        p.play().unwrap();
        assert!(
            p.pending().is_empty(),
            "a fresh graph took everything with it"
        );
        assert_eq!(p.apply().unwrap(), Applied::Nothing);
    }

    #[test]
    fn resume_gives_a_paused_edit_the_graph_it_needs() {
        let mut p: Player<Silent> = Player::default();
        p.add_track(track_with("a.wav", 10));
        let plays = |p: &Player<Silent>| {
            p.backend()
                .events
                .iter()
                .filter(|e| **e == BackendEvent::Play)
                .count()
        };
        p.play().unwrap();
        p.pause();

        // A gain lands on the paused graph: pausing stops the sound, not the
        // graph's ability to take a new value.
        assert_eq!(p.changed(Change::TrackGain(0)), Landed::Live);
        p.resume().unwrap();
        assert_eq!(plays(&p), 1, "so resuming re-plans nothing");

        // A structural edit cannot; resume must not drop it on the floor.
        p.pause();
        assert_eq!(p.changed(Change::Structure), Landed::Pending);
        p.resume().unwrap();
        assert_eq!(plays(&p), 2, "resume rebuilt for it");
        assert!(p.pending().is_empty());
        assert_eq!(
            p.backend().events.last(),
            Some(&BackendEvent::Resume),
            "and the rebuilt graph is running, not paused"
        );
    }

    /// A backend that can be told to refuse re-plans: what happens to edits
    /// that were waiting for one.
    #[derive(Debug, Default)]
    struct Refuses {
        refuse: bool,
    }

    impl Backend for Refuses {
        fn play(
            &mut self,
            _tracks: &[Track],
            _groups: &[Group],
            _at: Duration,
        ) -> Result<(), BackendError> {
            if self.refuse {
                return Err(BackendError::new("refuses", "no graph for you"));
            }
            Ok(())
        }
        fn pause(&mut self) {}
        fn resume(&mut self) {}
        fn stop(&mut self) {}
        fn set_volume(&mut self, _volume: f32) {}
    }

    #[test]
    fn a_failed_apply_keeps_what_it_could_not_land() {
        let mut p = Player::new(Refuses::default());
        p.add_track(track_with("a.wav", 10));
        p.play().unwrap();
        assert_eq!(p.changed(Change::Structure), Landed::Pending);

        p.backend_mut().refuse = true;
        assert!(p.apply().is_err());
        assert_eq!(
            p.pending(),
            &[Change::Structure],
            "a rebuild that did not happen leaves the edit waiting"
        );
        assert_eq!(p.state(), State::Playing, "and the transport untouched");

        p.backend_mut().refuse = false;
        assert_eq!(
            p.apply().unwrap(),
            Applied::Rebuilt {
                live: 0,
                at: Duration::ZERO
            },
            "so the next apply can still land it"
        );
        assert!(p.pending().is_empty());
    }

    #[test]
    fn stacked_tracks_mix_at_the_same_timecode() {
        let mut p: Player<Silent> = Player::default();
        let mut bed = Track::named("bed");
        bed.insert(Clip::new(
            Arc::new(bo_core::track::Source {
                uri: "bed.wav".into(),
            }),
            secs(30),
        ))
        .unwrap();
        p.add_track(bed);
        let mut ding = Track::named("ding");
        ding.insert(
            Clip::new(
                Arc::new(bo_core::track::Source {
                    uri: "ding.wav".into(),
                }),
                secs(2),
            )
            .at(secs(5)),
        )
        .unwrap();
        p.add_track(ding);

        assert_eq!(p.duration(), secs(30), "longest track wins");
        let at_zero: Vec<_> = p.active_clips(Duration::ZERO).map(|(i, _)| i).collect();
        assert_eq!(at_zero, vec![0]);
        let at_five: Vec<_> = p.active_clips(secs(5)).map(|(i, _)| i).collect();
        assert_eq!(
            at_five,
            vec![0, 1],
            "two sources at one timecode, on separate tracks"
        );
        assert_eq!(
            p.active_clips(secs(8)).map(|(i, _)| i).collect::<Vec<_>>(),
            vec![0],
            "the ding has ended"
        );
        assert!(p.active_clips(secs(31)).next().is_none());
    }

    #[test]
    fn a_finite_arrangement_finishes_when_the_playhead_passes_its_end() {
        let mut p: Player<Silent> = Player::default();
        p.add_track(track_with("a.wav", 10));
        p.play().unwrap();
        assert!(!p.is_finished());
        p.advance(secs(9));
        assert!(!p.is_finished(), "still inside the arrangement");
        p.advance(secs(2));
        assert!(p.is_finished(), "the playhead passed the end");
    }

    #[test]
    fn volume_is_clamped_and_forwarded() {
        let mut p: Player<Silent> = Player::default();
        p.set_volume(2.5);
        assert_eq!(p.volume(), 1.0);
        assert_eq!(p.backend().volume(), 1.0);
        p.set_volume(-1.0);
        assert_eq!(p.volume(), 0.0);
    }

    #[test]
    fn swapping_the_backend_resets_transport() {
        let mut p: Player<Silent> = Player::default();
        p.add_track(track_with("a.wav", 10));
        p.play().unwrap();
        p.seek(secs(3)).unwrap();
        let mut swapped = p.with_backend(Silent::default());
        assert_eq!(swapped.state(), State::Stopped);
        assert_eq!(swapped.tracks().len(), 1, "the arrangement came across");
        assert_eq!(swapped.playhead(), secs(3), "so did the position");
        assert!(
            swapped.backend().events.is_empty(),
            "the new one saw nothing"
        );
        assert_eq!(swapped.remove_track(0).map(|t| t.clips().len()), Some(1));
        assert_eq!(swapped.tracks().len(), 0);
    }

    #[test]
    fn reset_clears_tracks_and_transport() {
        let mut p: Player<Silent> = Player::default();
        p.add_track(track_with("a.wav", 10));
        p.play().unwrap();
        p.seek(Duration::from_secs(3)).unwrap();
        p.reset();
        assert_eq!(p.tracks().len(), 0);
        assert_eq!(p.state(), State::Stopped);
        assert_eq!(p.playhead(), Duration::ZERO);
        assert!(p.backend().events.contains(&BackendEvent::Stop));
    }

    #[test]
    fn the_group_table_lives_on_the_player_and_travels_with_play() {
        let mut p: Player<Silent> = Player::default();
        p.add_track(track_with("a.wav", 10));
        assert_eq!(p.add_group(Some("music".into())), 0);
        assert_eq!(p.add_group(Some("voice".into())), 1);
        assert_eq!(p.groups().len(), 2);
        assert_eq!(p.group(0).map(|g| g.name()), Some(Some("music")));
        assert!(p.group(9).is_none());

        // The bus table rides along on a plan, so a graph can bake its strips.
        p.play().unwrap();
        assert_eq!(p.backend().last_groups, Some(2));
        assert_eq!(p.backend().last_play, Some((1, Duration::ZERO)));

        // group_mut edits the strip the next plan will read.
        p.group_mut(1).expect("voice").set_muted(true);
        assert!(p.group(1).unwrap().muted());
    }

    #[test]
    fn a_group_gain_waits_for_apply_and_rebuilds() {
        let mut p: Player<Silent> = Player::default();
        p.add_track(track_with("a.wav", 10));
        let id = p.add_group(Some("music".into()));
        let plays = |p: &Player<Silent>| {
            p.backend()
                .events
                .iter()
                .filter(|e| **e == BackendEvent::Play)
                .count()
        };
        p.play().unwrap();
        assert_eq!(plays(&p), 1);

        // No running graph can retune a group strip live: the edit waits,
        // and apply gives it a rebuilt graph, exactly like a clip move.
        assert_eq!(p.changed(Change::GroupGain(id)), Landed::Pending);
        assert_eq!(p.pending(), &[Change::GroupGain(id)]);
        assert_eq!(
            p.apply().unwrap(),
            Applied::Rebuilt {
                live: 0,
                at: Duration::ZERO
            }
        );
        assert_eq!(plays(&p), 2);
        assert!(p.pending().is_empty());
    }

    #[test]
    fn reset_clears_the_group_table_and_ids_restart() {
        let mut p: Player<Silent> = Player::default();
        p.add_group(Some("music".into()));
        p.add_group(Some("voice".into()));
        p.reset();
        assert!(p.groups().is_empty());
        assert_eq!(p.add_group(None), 0, "a fresh session starts its ids at zero");
    }
}
