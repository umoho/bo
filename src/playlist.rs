//! The playlist model — the abstraction every other part of `bo` talks to.
//!
//! This module is deliberately free of audio, I/O and terminal concerns. It is a
//! pure, deterministic data model that both the interactive CLI and an agent can
//! reason about:
//!
//! * [`Track`] — an entry: an addressable stream plus lazily-resolved metadata.
//! * [`Playlist`] — an ordered bag of tracks with a *playback cursor*, a
//!   [`PlayMode`] (shuffle / [`Repeat`]) and a bounded history for `back()`.
//! * [`Nav`] — the result of a navigation step, so a caller can tell "moved",
//!   "wrapped", "requeued the same track" and "ran out of tracks" apart.
//!
//! Two orderings exist at once, which is the whole reason this is a struct and
//! not a `Vec`:
//!
//! * **user order** — `0..len`, the list as the user (or agent) sees and indexes
//!   it. Every mutating API speaks this index.
//! * **play order** — a permutation of those indices, held in `order`; the
//!   cursor is a *position* in it. Unshuffled the two coincide; shuffled they
//!   diverge, and the permutation is redrawn from a seed an agent can pin.
//!
//! Invariants maintained by this module (checked in debug builds):
//!
//! 1. `order` is always a permutation of `0..tracks.len()` — no gaps, no dupes.
//! 2. `cursor` is `None` or a valid position in `order`.
//! 3. Structural edits keep the cursor on the same *track* where possible, so
//!    removing an entry ahead of the cursor does not skip the following one.
//! 4. Turning shuffle off restores the identity permutation — play order only
//!    ever diverges from user order while shuffle is on.

use std::collections::VecDeque;
use std::fmt;
use std::mem;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// How many past play positions [`Playlist::back`] can retrace.
pub const HISTORY_LIMIT: usize = 64;

// ---------------------------------------------------------------------------
// Ids and errors
// ---------------------------------------------------------------------------

/// Process-unique identity of a [`Track`].
///
/// Indices shift when the playlist is edited; ids do not. Agents should quote ids
/// when referring to an entry across turns, and indices only for positional
/// commands ("delete the third one").
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TrackId(u64);

static NEXT_ID: AtomicU64 = AtomicU64::new(1);

impl TrackId {
    /// Allocate the next id.
    #[must_use]
    pub fn next() -> Self {
        Self(NEXT_ID.fetch_add(1, Ordering::Relaxed))
    }

    /// Wrap a raw value, e.g. one restored from a saved playlist.
    #[must_use]
    pub const fn from_raw(raw: u64) -> Self {
        Self(raw)
    }

    /// The underlying integer.
    #[must_use]
    pub const fn as_raw(self) -> u64 {
        self.0
    }
}

impl fmt::Display for TrackId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "#{}", self.0)
    }
}

/// Everything that can go wrong when addressing an entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// The playlist holds no entries.
    Empty,
    /// An index was outside `0..len`.
    IndexOutOfBounds {
        /// The index the caller supplied.
        index: usize,
        /// Current length of the playlist.
        len: usize,
    },
    /// A proposed ordering was not a permutation of `0..len`.
    BadLayout {
        /// Expected number of entries.
        len: usize,
    },
    /// No entry carries this id (it was removed, or belongs to another playlist).
    NotFound {
        /// The id that could not be resolved.
        id: TrackId,
    },
}

impl Error {
    fn out_of_bounds(index: usize, len: usize) -> Self {
        Self::IndexOutOfBounds { index, len }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => write!(f, "playlist is empty"),
            Self::IndexOutOfBounds { index, len } => {
                write!(f, "index {index} out of bounds for {len} track(s)")
            }
            Self::BadLayout { len } => write!(f, "ordering must be a permutation of 0..{len}"),
            Self::NotFound { id } => write!(f, "no entry with id {id} in this playlist"),
        }
    }
}

impl std::error::Error for Error {}

/// Result alias for playlist operations.
pub type Result<T> = std::result::Result<T, Error>;

// ---------------------------------------------------------------------------
// Track
// ---------------------------------------------------------------------------

/// A single entry: something playable plus the metadata used to render and search it.
///
/// Metadata is optional by design — a URI is always enough to queue something,
/// and tags can be filled in later (`with_title`, `with_artist`, …) without the
/// cursor or play order being disturbed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Track {
    id: TrackId,
    uri: String,
    title: String,
    artist: Option<String>,
    album: Option<String>,
    duration: Option<Duration>,
}

impl Track {
    /// Queue `uri`, guessing the title from its last path segment.
    pub fn new(uri: impl Into<String>) -> Self {
        let uri = uri.into();
        let title = derive_title(&uri);
        Self {
            id: TrackId::next(),
            uri,
            title,
            artist: None,
            album: None,
            duration: None,
        }
    }

    /// Reattach a known id — used when rehydrating a saved playlist.
    #[must_use]
    pub fn with_id(mut self, id: TrackId) -> Self {
        self.id = id;
        self
    }

    /// Set the title.
    #[must_use]
    pub fn with_title(mut self, title: impl Into<String>) -> Self {
        self.title = title.into();
        self
    }

    /// Set the artist.
    #[must_use]
    pub fn with_artist(mut self, artist: impl Into<String>) -> Self {
        self.artist = Some(artist.into());
        self
    }

    /// Set the album.
    #[must_use]
    pub fn with_album(mut self, album: impl Into<String>) -> Self {
        self.album = Some(album.into());
        self
    }

    /// Set the duration (`None` = unknown or live).
    #[must_use]
    pub fn with_duration(mut self, duration: impl Into<Option<Duration>>) -> Self {
        self.duration = duration.into();
        self
    }

    /// Replace the title in place — for tags that resolve after queueing.
    pub fn set_title(&mut self, title: impl Into<String>) {
        self.title = title.into();
    }

    /// Replace the artist in place.
    pub fn set_artist(&mut self, artist: impl Into<String>) {
        self.artist = Some(artist.into());
    }

    /// Replace the album in place.
    pub fn set_album(&mut self, album: impl Into<String>) {
        self.album = Some(album.into());
    }

    /// Replace the duration in place.
    pub fn set_duration(&mut self, duration: impl Into<Option<Duration>>) {
        self.duration = duration.into();
    }

    /// Stable identity of this entry.
    #[must_use]
    pub const fn id(&self) -> TrackId {
        self.id
    }

    /// Where to read the audio from (file path, URL, device, …).
    #[must_use]
    pub fn uri(&self) -> &str {
        &self.uri
    }

    /// Display title (never empty; falls back to the URI stem).
    #[must_use]
    pub fn title(&self) -> &str {
        &self.title
    }

    /// Artist, when known.
    #[must_use]
    pub fn artist(&self) -> Option<&str> {
        self.artist.as_deref()
    }

    /// Album, when known.
    #[must_use]
    pub fn album(&self) -> Option<&str> {
        self.album.as_deref()
    }

    /// Declared duration, when known.
    #[must_use]
    pub const fn duration(&self) -> Option<Duration> {
        self.duration
    }

    /// True when the track has no known end (live stream, unresolved probe).
    #[must_use]
    pub const fn is_open_ended(&self) -> bool {
        self.duration.is_none()
    }

    /// `"artist — title"`, or just the title when the artist is unknown.
    #[must_use]
    pub fn display_line(&self) -> String {
        match self.artist() {
            Some(artist) => format!("{artist} \u{2014} {}", self.title),
            None => self.title.clone(),
        }
    }

    /// Right-aligned length column: `"mm:ss"`, `"h:mm:ss"` or `"--:--"`.
    #[must_use]
    pub fn duration_label(&self) -> String {
        match self.duration {
            Some(d) => format_duration(d),
            None => "--:--".into(),
        }
    }

    /// Relevance of `needle` against this title/artist/album/uri, case-insensitive.
    ///
    /// Lower is better; `None` means no match. Used by [`Playlist::search`].
    #[must_use]
    pub fn relevance(&self, needle: &str) -> Option<u8> {
        let lowered = needle.trim().to_lowercase();
        let needle = lowered.as_str();
        if needle.is_empty() {
            return Some(0);
        }
        let title = self.title.to_lowercase();
        if title == needle {
            return Some(0);
        }
        if title.starts_with(needle) {
            return Some(1);
        }
        if title.contains(needle) {
            return Some(2);
        }
        if self.artist().is_some_and(|a| a.to_lowercase() == needle) {
            return Some(3);
        }
        if self
            .artist()
            .is_some_and(|a| a.to_lowercase().contains(needle))
            || self
                .album()
                .is_some_and(|al| al.to_lowercase().contains(needle))
        {
            return Some(4);
        }
        if self.uri.to_lowercase().contains(needle) {
            return Some(5);
        }
        None
    }
}

/// Format a duration as `mm:ss`, or `h:mm:ss` past an hour.
#[must_use]
pub fn format_duration(duration: Duration) -> String {
    let total = duration.as_secs();
    let (h, m, s) = (total / 3600, (total % 3600) / 60, total % 60);
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m:02}:{s:02}")
    }
}

fn derive_title(uri: &str) -> String {
    let path = uri.split(['?', '#']).next().unwrap_or(uri);
    let path = path.trim_end_matches(['/', '\\']);
    let last = path.rsplit(['/', '\\']).next().unwrap_or("");
    let stem = match last.rfind('.') {
        None | Some(0) => last,
        Some(dot) => &last[..dot],
    };
    if stem.trim().is_empty() {
        "unknown".to_owned()
    } else {
        stem.to_owned()
    }
}

impl fmt::Display for Track {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} [{}]", self.display_line(), self.duration_label())
    }
}

// ---------------------------------------------------------------------------
// Mode
// ---------------------------------------------------------------------------

/// Repeat policy.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Repeat {
    /// Stop at the end of the playlist.
    #[default]
    Off,
    /// Requeue the current entry forever.
    One,
    /// Wrap around to the first entry.
    All,
}

impl fmt::Display for Repeat {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Off => "repeat off",
            Self::One => "repeat one",
            Self::All => "repeat all",
        })
    }
}

/// Playback policy. Plain data so an agent can read, tweak and set it wholesale.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PlayMode {
    /// Play in a random permutation instead of user order.
    pub shuffle: bool,
    /// What happens at the end of the playlist.
    pub repeat: Repeat,
}

impl PlayMode {
    /// Plain sequential play, no repeat.
    pub const SEQUENTIAL: Self = Self {
        shuffle: false,
        repeat: Repeat::Off,
    };

    /// Sequential play that wraps.
    pub const REPEAT_ALL: Self = Self {
        shuffle: false,
        repeat: Repeat::All,
    };

    /// Shuffled play that wraps.
    pub const SHUFFLE_ALL: Self = Self {
        shuffle: true,
        repeat: Repeat::All,
    };

    /// A one-shuffle mode value.
    #[must_use]
    pub const fn shuffled(shuffle: bool) -> Self {
        Self {
            shuffle,
            repeat: Repeat::Off,
        }
    }

    /// A repeat mode value in user order.
    #[must_use]
    pub const fn repeating(repeat: Repeat) -> Self {
        Self {
            shuffle: false,
            repeat,
        }
    }

    /// Human-readable summary for the status line.
    #[must_use]
    pub fn label(&self) -> String {
        match (self.shuffle, self.repeat) {
            (false, Repeat::Off) => "in order".into(),
            (false, r) => r.to_string(),
            (true, Repeat::Off) => "shuffle".into(),
            (true, r) => format!("shuffle \u{b7} {r}"),
        }
    }
}

impl fmt::Display for PlayMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.label())
    }
}

/// Outcome of a navigation step.
///
/// The distinction matters to the player engine: `Moved` means "load and play",
/// `Same` means "restart this track", `Wrapped` means "load, and it's a new pass",
/// `Ended` means "idle, wait for the user or the agent".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Nav {
    /// Moved to a different entry.
    Moved {
        /// New position in the play order.
        position: usize,
        /// Index of the entry in user order.
        index: usize,
    },
    /// Moved to a different entry, wrapping past the end.
    Wrapped {
        /// New position in the play order (always the first).
        position: usize,
        /// Index of the entry in user order.
        index: usize,
    },
    /// Stayed put (repeat-one, or already at the requested entry).
    Same {
        /// Position in the play order.
        position: usize,
        /// Index of the entry in user order.
        index: usize,
    },
    /// Nothing left to play.
    Ended,
}

impl Nav {
    /// Play-order position of the resulting entry, if any.
    #[must_use]
    pub const fn position(self) -> Option<usize> {
        match self {
            Self::Moved { position, .. }
            | Self::Wrapped { position, .. }
            | Self::Same { position, .. } => Some(position),
            Self::Ended => None,
        }
    }

    /// User-order index of the resulting entry, if any.
    #[must_use]
    pub const fn index(self) -> Option<usize> {
        match self {
            Self::Moved { index, .. } | Self::Wrapped { index, .. } | Self::Same { index, .. } => {
                Some(index)
            }
            Self::Ended => None,
        }
    }

    /// Whether the playlist ran dry.
    #[must_use]
    pub const fn ended(self) -> bool {
        matches!(self, Self::Ended)
    }

    /// Whether playback continues on a track.
    #[must_use]
    pub const fn played(self) -> bool {
        !self.ended()
    }
}

impl fmt::Display for Nav {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Moved { position, index } => {
                write!(f, "play track {index} at position {position}")
            }
            Self::Wrapped { position, index } => {
                write!(f, "wrap to track {index} at position {position}")
            }
            Self::Same { index, .. } => write!(f, "requeue track {index}"),
            Self::Ended => write!(f, "end of playlist"),
        }
    }
}

// ---------------------------------------------------------------------------
// Deterministic RNG (shuffles must be reproducible for an agent to test them)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Self(mix(seed) | 1)
    }

    fn next_u64(&mut self) -> u64 {
        // xorshift64*
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    /// Uniform value in `0..bound` (rejection-free enough for list lengths here).
    fn below(&mut self, bound: usize) -> usize {
        if bound == 0 {
            return 0;
        }
        (self.next_u64() % bound as u64) as usize
    }

    fn shuffle<T>(&mut self, slice: &mut [T]) {
        for i in (1..slice.len()).rev() {
            let j = self.below(i + 1);
            slice.swap(i, j);
        }
    }
}

fn mix(mut x: u64) -> u64 {
    x = x.wrapping_add(0x9E37_79B9_7F4A_7C15);
    x ^= x >> 30;
    x = x.wrapping_mul(0xBF58_476D_1CE4_E5B9);
    x ^= x >> 27;
    x = x.wrapping_mul(0x94D0_49BB_1331_11EB);
    x ^ (x >> 31)
}

fn system_seed() -> u64 {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    mix(nanos ^ (NEXT_ID.load(Ordering::Relaxed) << 17))
}

// ---------------------------------------------------------------------------
// Playlist
// ---------------------------------------------------------------------------

/// An ordered set of tracks with a playback cursor.
#[derive(Debug, Clone)]
pub struct Playlist {
    name: String,
    tracks: Vec<Track>,
    /// Permutation of `0..tracks.len()`: the play order.
    order: Vec<usize>,
    /// Position in `order` of the current entry; `None` until something plays.
    cursor: Option<usize>,
    /// Recently played positions, oldest first, for [`Playlist::back`].
    history: VecDeque<usize>,
    mode: PlayMode,
    rng: Rng,
    seed: u64,
}

impl Default for Playlist {
    fn default() -> Self {
        Self::new()
    }
}

impl Playlist {
    /// An empty, unnamed playlist.
    #[must_use]
    pub fn new() -> Self {
        Self::named("playlist")
    }

    /// An empty playlist with a name, for the header line.
    #[must_use]
    pub fn named(name: impl Into<String>) -> Self {
        let seed = system_seed();
        Self {
            name: name.into(),
            tracks: Vec::new(),
            order: Vec::new(),
            cursor: None,
            history: VecDeque::new(),
            mode: PlayMode::default(),
            rng: Rng::new(seed),
            seed,
        }
    }

    /// A playlist in user order from any iterator of tracks.
    #[must_use]
    pub fn from_tracks(tracks: impl IntoIterator<Item = Track>) -> Self {
        tracks.into_iter().collect()
    }

    /// A playlist with a name and an explicit shuffle seed, so play order is
    /// reproducible. This is the constructor to use in tests.
    #[must_use]
    pub fn seeded(name: impl Into<String>, seed: u64) -> Self {
        let mut pl = Self::named(name);
        pl.seed = seed;
        pl.rng = Rng::new(seed);
        pl
    }

    // -- identity / mode ----------------------------------------------------

    /// Header name.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Rename the playlist.
    pub fn set_name(&mut self, name: impl Into<String>) {
        self.name = name.into();
    }

    /// Current playback policy.
    #[must_use]
    pub fn mode(&self) -> PlayMode {
        self.mode
    }

    /// Set the whole policy at once; returns whether anything changed.
    pub fn set_mode(&mut self, mode: PlayMode) -> bool {
        let changed = self.mode != mode;
        let repeat = mode.repeat;
        self.mode.repeat = repeat;
        if self.mode.shuffle != mode.shuffle {
            self.set_shuffle(mode.shuffle);
        }
        changed
    }

    /// Current repeat policy.
    #[must_use]
    pub fn repeat(&self) -> Repeat {
        self.mode.repeat
    }

    /// Set the repeat policy.
    ///
    /// Turning repeat-one off while it is active does not move the cursor; the
    /// next [`Playlist::advance`] steps on as usual.
    pub fn set_repeat(&mut self, repeat: Repeat) -> Repeat {
        mem::replace(&mut self.mode.repeat, repeat)
    }

    /// Whether shuffle is on.
    #[must_use]
    pub fn is_shuffled(&self) -> bool {
        self.mode.shuffle
    }

    /// Turn shuffle on or off, keeping the current track playing.
    ///
    /// Going *on*: the current entry is pinned to its current play position and
    /// the rest is shuffled around it, so the track never restarts or skips.
    /// Going *off*: the play order collapses back to user order and the cursor
    /// lands on the current entry's own index. Returns the previous value.
    ///
    /// Newly appended entries always join at the end of the play order, so
    /// queueing something never changes what plays next.
    pub fn set_shuffle(&mut self, shuffle: bool) -> bool {
        let previous = mem::replace(&mut self.mode.shuffle, shuffle);
        if previous != shuffle {
            let current = self.current_index();
            let anchor = self.cursor;
            self.resequence(current, anchor);
        }
        previous
    }

    /// Toggle shuffle, returning the new value.
    pub fn toggle_shuffle(&mut self) -> bool {
        let next = !self.mode.shuffle;
        self.set_shuffle(next);
        next
    }

    /// The seed new shuffles are derived from.
    #[must_use]
    pub fn shuffle_seed(&self) -> u64 {
        self.seed
    }

    /// Fix the shuffle seed and reshuffle (current track stays put).
    pub fn set_shuffle_seed(&mut self, seed: u64) {
        self.seed = seed;
        self.rng = Rng::new(seed);
        let current = self.current_index();
        let anchor = self.cursor;
        self.resequence(current, anchor);
    }

    /// Re-draw the shuffle with a fresh seed.
    pub fn reshuffle(&mut self) {
        self.set_shuffle_seed(system_seed());
    }

    // -- inspection ---------------------------------------------------------

    /// Number of entries.
    #[must_use]
    pub fn len(&self) -> usize {
        self.tracks.len()
    }

    /// Whether there is nothing to play.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.tracks.is_empty()
    }

    /// Entries in user order.
    #[must_use]
    pub fn tracks(&self) -> &[Track] {
        &self.tracks
    }

    /// Entries in user order, mutable (e.g. to fill in tags after a probe).
    pub fn tracks_mut(&mut self) -> &mut [Track] {
        &mut self.tracks
    }

    /// The play order: a permutation of user-order indices.
    ///
    /// `order[p]` is the entry played at position `p`. Exposed read-only for
    /// rendering, debugging and agent introspection.
    #[must_use]
    pub fn play_order(&self) -> &[usize] {
        &self.order
    }

    /// Iterate entries in user order.
    pub fn iter(&self) -> impl Iterator<Item = &Track> {
        self.tracks.iter()
    }

    /// Iterate entries in play order.
    pub fn iter_playback(&self) -> impl Iterator<Item = &Track> {
        self.order.iter().map(move |&i| &self.tracks[i])
    }

    /// Entry at a user-order index.
    #[must_use]
    pub fn get(&self, index: usize) -> Option<&Track> {
        self.tracks.get(index)
    }

    /// Mutable entry at a user-order index.
    pub fn get_mut(&mut self, index: usize) -> Option<&mut Track> {
        self.tracks.get_mut(index)
    }

    /// User-order index of an id, if still present.
    #[must_use]
    pub fn index_of(&self, id: TrackId) -> Option<usize> {
        self.tracks.iter().position(|t| t.id() == id)
    }

    /// Play position of a user-order index.
    #[must_use]
    pub fn position_of(&self, index: usize) -> Option<usize> {
        self.order.iter().position(|&i| i == index)
    }

    /// User-order index at a play position.
    #[must_use]
    pub fn index_at(&self, position: usize) -> Option<usize> {
        self.order.get(position).copied()
    }

    /// Index of the current entry in user order.
    #[must_use]
    pub fn current_index(&self) -> Option<usize> {
        self.cursor.and_then(|p| self.order.get(p).copied())
    }

    /// Current play position.
    #[must_use]
    pub fn current_position(&self) -> Option<usize> {
        self.cursor
    }

    /// The entry that is current, if playback has started.
    #[must_use]
    pub fn current(&self) -> Option<&Track> {
        self.current_index().map(|i| &self.tracks[i])
    }

    /// Whether the cursor sits on the last entry of the play order.
    ///
    /// An empty or unstarted playlist is *not* finished — there is nothing that
    /// could still be played, but no pass has been completed either.
    #[must_use]
    pub fn is_finished(&self) -> bool {
        self.cursor.is_some() && self.cursor == self.order.len().checked_sub(1)
    }

    /// How many entries are queued ahead of the cursor.
    ///
    /// Before playback starts this counts the whole playlist.
    #[must_use]
    pub fn remaining(&self) -> usize {
        match self.cursor {
            None => self.order.len(),
            Some(p) => self.order.len().saturating_sub(p + 1),
        }
    }

    /// The entry [`Playlist::advance`] would land on, honouring the mode.
    #[must_use]
    pub fn peek_next(&self) -> Option<&Track> {
        if self.order.is_empty() {
            return None;
        }
        let Some(pos) = self.cursor else {
            return Some(&self.tracks[self.order[0]]);
        };
        if self.mode.repeat == Repeat::One {
            return Some(&self.tracks[self.order[pos]]);
        }
        let next = if pos + 1 < self.order.len() {
            pos + 1
        } else if self.mode.repeat == Repeat::All {
            0
        } else {
            return None;
        };
        Some(&self.tracks[self.order[next]])
    }

    /// Sum of known durations, and how many entries have none.
    #[must_use]
    pub fn duration_known(&self) -> (Duration, usize) {
        let mut total = Duration::ZERO;
        let mut unknown = 0;
        for t in &self.tracks {
            match t.duration() {
                Some(d) => total += d,
                None => unknown += 1,
            }
        }
        (total, unknown)
    }

    /// Total length, or `None` when any entry's duration is unknown.
    #[must_use]
    pub fn duration_total(&self) -> Option<Duration> {
        let (total, unknown) = self.duration_known();
        (unknown == 0 && !self.tracks.is_empty()).then_some(total)
    }

    /// Indices of entries matching `query` (case-insensitive, trimmed), best
    /// match first; see [`Track::relevance`] for the ranking.
    ///
    /// An empty query returns every index, i.e. the whole list.
    #[must_use]
    pub fn search(&self, query: &str) -> Vec<usize> {
        let mut hits: Vec<(u8, usize)> = self
            .tracks
            .iter()
            .enumerate()
            .filter_map(|(i, t)| t.relevance(query).map(|rank| (rank, i)))
            .collect();
        hits.sort_by_key(|(rank, index)| (*rank, *index));
        hits.into_iter().map(|(_, index)| index).collect()
    }

    /// First entry matching the predicate.
    #[must_use]
    pub fn find<P>(&self, predicate: P) -> Option<usize>
    where
        P: FnMut(&Track) -> bool,
    {
        self.tracks.iter().position(predicate)
    }

    /// Distinct artist labels, in first-seen order.
    #[must_use]
    pub fn artists(&self) -> Vec<&str> {
        let mut out: Vec<&str> = Vec::new();
        for t in &self.tracks {
            if let Some(a) = t.artist()
                && !out.contains(&a)
            {
                out.push(a);
            }
        }
        out
    }

    // -- editing ------------------------------------------------------------

    /// Append an entry. Returns its user-order index.
    ///
    /// New entries join at the *end of the play order* — even under shuffle — so
    /// adding something never changes what plays next.
    pub fn push(&mut self, track: Track) -> usize {
        let index = self.tracks.len();
        self.tracks.push(track);
        self.order.push(index);
        self.debug_check();
        index
    }

    /// Append many entries. Returns how many were added.
    pub fn append_tracks(&mut self, tracks: impl IntoIterator<Item = Track>) -> usize {
        let mut n = 0;
        for t in tracks {
            self.push(t);
            n += 1;
        }
        n
    }

    /// Merge another playlist into this one in its user order, emptying it.
    /// Returns the index the first moved entry landed on.
    pub fn merge(&mut self, other: &mut Self) -> Option<usize> {
        if other.is_empty() {
            return None;
        }
        let base = self.tracks.len();
        let moved_order = mem::take(&mut other.order);
        other.cursor = None;
        other.history.clear();
        self.tracks.extend(mem::take(&mut other.tracks));
        self.order.extend(moved_order.iter().map(|&i| base + i));
        self.debug_check();
        Some(base)
    }

    /// Insert at a user-order index; `at == len()` appends.
    pub fn insert(&mut self, at: usize, track: Track) -> Result<usize> {
        if at > self.tracks.len() {
            return Err(Error::out_of_bounds(at, self.tracks.len()));
        }
        self.tracks.insert(at, track);
        for slot in &mut self.order {
            if *slot >= at {
                *slot += 1;
            }
        }
        self.order.push(at);
        self.debug_check();
        Ok(at)
    }

    /// Remove the entry at a user-order index.
    ///
    /// The cursor follows the track it was on; if that track is the one removed,
    /// the cursor stays on the same *position*, which now holds its successor —
    /// the behaviour of every real player's "skip and delete".
    pub fn remove(&mut self, index: usize) -> Option<Track> {
        if index >= self.tracks.len() {
            return None;
        }
        let removed = self.tracks.remove(index);
        let dropped = mem::replace(&mut self.order, Vec::with_capacity(self.tracks.len()));
        let mut next = Vec::with_capacity(self.tracks.len());
        let mut dropped_position: Option<usize> = None;
        for old in dropped {
            if old == index {
                dropped_position = Some(next.len());
                continue;
            }
            next.push(if old > index { old - 1 } else { old });
        }
        self.order = next;
        if let Some(removed_pos) = dropped_position {
            self.drop_position(removed_pos);
        }
        self.debug_check();
        Some(removed)
    }

    /// Remove an entry by id. Returns its old index alongside the track.
    pub fn remove_id(&mut self, id: TrackId) -> Option<(usize, Track)> {
        let index = self.index_of(id)?;
        self.remove(index).map(|t| (index, t))
    }

    /// Drop every entry, keeping the name, mode and seed.
    pub fn clear(&mut self) {
        self.tracks.clear();
        self.order.clear();
        self.cursor = None;
        self.history.clear();
        self.debug_check();
    }

    /// Retain entries matching a predicate. Returns how many were dropped.
    pub fn retain<P>(&mut self, mut keep: P) -> usize
    where
        P: FnMut(&Track) -> bool,
    {
        let layout: Vec<usize> = self
            .tracks
            .iter()
            .enumerate()
            .filter(|(_, t)| keep(t))
            .map(|(i, _)| i)
            .collect();
        let before = self.tracks.len();
        if layout.len() == before {
            return 0;
        }
        let current = self.current_index();
        let anchor = self.cursor;
        self.relayout(layout);
        // The anchored position may refer to a track that no longer exists.
        if self.current_index() != current {
            self.cursor = anchor.filter(|&p| p < self.order.len());
        }
        self.debug_check();
        before - self.tracks.len()
    }

    /// Swap two entries in user order.
    pub fn swap(&mut self, a: usize, b: usize) -> Result<()> {
        let len = self.tracks.len();
        if a >= len || b >= len {
            return Err(Error::out_of_bounds(if a >= len { a } else { b }, len));
        }
        if a == b {
            return Ok(());
        }
        self.tracks.swap(a, b);
        for slot in &mut self.order {
            *slot = match *slot {
                x if x == a => b,
                x if x == b => a,
                x => x,
            };
        }
        self.debug_check();
        Ok(())
    }

    /// Move an entry to a new user-order index.
    pub fn move_track(&mut self, from: usize, to: usize) -> Result<()> {
        let len = self.tracks.len();
        if from >= len || to >= len {
            return Err(Error::out_of_bounds(
                if from >= len { from } else { to },
                len,
            ));
        }
        if from == to {
            return Ok(());
        }
        let mut layout: Vec<usize> = (0..len).collect();
        let moved = layout.remove(from);
        layout.insert(to, moved);
        self.relayout(layout);
        self.debug_check();
        Ok(())
    }

    /// Replace the user order with `layout` (a permutation of `0..len`).
    ///
    /// This is the "apply the agent's reordering" entry point; play order and
    /// cursor are carried along, so the current track keeps playing.
    pub fn reorder(&mut self, layout: &[usize]) -> Result<()> {
        let len = self.tracks.len();
        if layout.len() != len {
            return Err(Error::BadLayout { len });
        }
        let mut seen = vec![false; len];
        for &old in layout {
            if old >= len || mem::replace(&mut seen[old], true) {
                return Err(Error::BadLayout { len });
            }
        }
        self.relayout(layout.to_vec());
        self.debug_check();
        Ok(())
    }

    /// Sort entries by a comparator, keeping the current track playing.
    pub fn sort_by<F>(&mut self, mut compare: F)
    where
        F: FnMut(&Track, &Track) -> std::cmp::Ordering,
    {
        let mut layout: Vec<usize> = (0..self.tracks.len()).collect();
        layout.sort_by(|&a, &b| compare(&self.tracks[a], &self.tracks[b]));
        self.relayout(layout);
        self.debug_check();
    }

    // -- navigation ---------------------------------------------------------

    /// Jump to an entry by user-order index.
    pub fn jump_to(&mut self, index: usize) -> Result<Nav> {
        if index >= self.tracks.len() {
            return Err(Error::out_of_bounds(index, self.tracks.len()));
        }
        let position = self.position_of(index).unwrap_or(0);
        Ok(self.set_cursor(position, true))
    }

    /// Jump to an entry by id.
    pub fn jump_to_id(&mut self, id: TrackId) -> Result<Nav> {
        let index = self.index_of(id).ok_or(Error::NotFound { id })?;
        self.jump_to(index)
    }

    /// Jump to a position in the play order.
    pub fn jump_to_position(&mut self, position: usize) -> Result<Nav> {
        if position >= self.order.len() {
            return Err(Error::out_of_bounds(position, self.order.len()));
        }
        Ok(self.set_cursor(position, true))
    }

    /// Move the cursor to the first entry of the play order.
    pub fn first(&mut self) -> Option<Nav> {
        if self.order.is_empty() {
            return None;
        }
        Some(self.set_cursor(0, true))
    }

    /// Move the cursor to the last entry of the play order.
    pub fn last(&mut self) -> Option<Nav> {
        let last = self.order.len().checked_sub(1)?;
        Some(self.set_cursor(last, true))
    }

    /// Step forward, as a finished track would.
    ///
    /// From a stopped cursor this starts at the beginning. Under
    /// [`Repeat::One`] it requeues the current entry; at the end of the play
    /// order it wraps or ends according to the repeat policy. A cursor left at
    /// the end is *not* sticky: if new entries have since been queued, or repeat
    /// turned on, `advance` walks on.
    pub fn advance(&mut self) -> Nav {
        if self.order.is_empty() {
            return Nav::Ended;
        }
        let Some(position) = self.cursor else {
            return self.set_cursor(0, false);
        };
        if self.mode.repeat == Repeat::One {
            return Nav::Same {
                position,
                index: self.order[position],
            };
        }
        if position + 1 < self.order.len() {
            self.remember(position);
            return self.set_cursor(position + 1, false);
        }
        if self.mode.repeat == Repeat::All {
            self.remember(position);
            let index = self.order[0];
            self.cursor = Some(0);
            self.debug_check();
            return Nav::Wrapped { position: 0, index };
        }
        self.debug_check();
        Nav::Ended
    }

    /// Step back, retracing history first (so "prev" undoes a shuffled jump) and
    /// falling back to the previous play position.
    pub fn back(&mut self) -> Nav {
        if self.order.is_empty() {
            return Nav::Ended;
        }
        let Some(position) = self.cursor else {
            return self.set_cursor(0, false);
        };
        if let Some(previous) = self.history.pop_back() {
            let previous = previous.min(self.order.len() - 1);
            if previous == position {
                // Undoing a manual jump: don't bounce on the spot.
                return Nav::Same {
                    position,
                    index: self.order[position],
                };
            }
            return self.set_cursor(previous, false);
        }
        if position > 0 {
            return self.set_cursor(position - 1, false);
        }
        if self.mode.repeat == Repeat::All {
            let last = self.order.len() - 1;
            let index = self.order[last];
            self.cursor = Some(last);
            self.debug_check();
            return Nav::Wrapped {
                position: last,
                index,
            };
        }
        Nav::Same {
            position: 0,
            index: self.order[0],
        }
    }

    /// Forget the cursor without touching the entries (transport stopped).
    pub fn reset(&mut self) {
        self.cursor = None;
        self.history.clear();
    }

    /// Positions that [`Playlist::back`] can still retrace.
    #[must_use]
    pub fn history_len(&self) -> usize {
        self.history.len()
    }

    // -- internals ----------------------------------------------------------

    fn remember(&mut self, position: usize) {
        self.history.push_back(position);
        while self.history.len() > HISTORY_LIMIT {
            self.history.pop_front();
        }
    }

    /// Park the cursor on `position` (clamped), reporting what happened.
    ///
    /// `remember` marks this as a deliberate jump: the position being left is
    /// pushed onto history so [`Playlist::back`] can undo the jump.
    fn set_cursor(&mut self, position: usize, remember: bool) -> Nav {
        debug_assert!(
            !self.order.is_empty(),
            "cannot move a cursor with no entries"
        );
        let position = position.min(self.order.len() - 1);
        let index = self.order[position];
        let staying = self.cursor == Some(position);
        if remember
            && !staying
            && let Some(previous) = self.cursor
        {
            self.remember(previous);
        }
        self.cursor = Some(position);
        self.debug_check();
        if staying {
            Nav::Same { position, index }
        } else {
            Nav::Moved { position, index }
        }
    }

    /// Adjust cursor and history after the play position `dropped` disappeared.
    fn drop_position(&mut self, dropped: usize) {
        if let Some(position) = self.cursor {
            self.cursor = if self.order.is_empty() {
                None
            } else if position > dropped {
                Some(position - 1)
            } else if position == dropped {
                Some(position.min(self.order.len() - 1))
            } else {
                Some(position)
            };
        }
        if self.order.is_empty() {
            self.history.clear();
            return;
        }
        let limit = self.order.len();
        let mut history = mem::take(&mut self.history);
        let mut rebuilt = VecDeque::with_capacity(history.len());
        while let Some(p) = history.pop_front() {
            if p == dropped {
                continue;
            }
            let p = if p > dropped { p - 1 } else { p };
            if p < limit {
                rebuilt.push_back(p);
            }
        }
        self.history = rebuilt;
    }

    fn build_order(&mut self) -> Vec<usize> {
        let mut order: Vec<usize> = (0..self.tracks.len()).collect();
        if self.mode.shuffle {
            self.rng.shuffle(&mut order);
        }
        order
    }

    /// Rebuild `order` for the current mode, pinning `current` to position `anchor`.
    ///
    /// Pinning only applies to shuffle: in user order the play order *is* the
    /// identity permutation, so the current entry sits wherever it sorts.
    fn resequence(&mut self, current: Option<usize>, anchor: Option<usize>) {
        self.order = self.build_order();
        let last = match self.order.len() {
            0 => {
                self.cursor = None;
                self.history.clear();
                return;
            }
            n => n - 1,
        };
        match current {
            Some(index) if self.mode.shuffle => {
                let from = self.order.iter().position(|&i| i == index).unwrap_or(0);
                self.order.remove(from);
                let to = anchor.unwrap_or(index).min(self.order.len());
                self.order.insert(to, index);
                self.cursor = Some(to);
            }
            Some(index) => {
                self.cursor = Some(index.min(last));
            }
            None => {
                self.cursor = anchor.map(|p| p.min(last));
            }
        }
        self.history.retain(|&p| p < self.order.len());
        self.debug_check();
    }

    /// Rebuild `tracks` so that new slot `j` holds old index `layout[j]`,
    /// remapping play order, cursor and history along the way.
    fn relayout(&mut self, layout: Vec<usize>) {
        let old_len = self.tracks.len();
        let mut slots: Vec<Option<Track>> =
            mem::take(&mut self.tracks).into_iter().map(Some).collect();
        let mut old_to_new = vec![usize::MAX; old_len];
        let mut tracks = Vec::with_capacity(layout.len());
        for (new, old) in layout.into_iter().enumerate() {
            old_to_new[old] = new;
            tracks.push(
                slots[old]
                    .take()
                    .unwrap_or_else(|| panic!("playlist layout reused index {old}")),
            );
        }
        self.tracks = tracks;
        self.order = self
            .order
            .iter()
            .filter_map(|&old| {
                let new = old_to_new[old];
                (new != usize::MAX).then_some(new)
            })
            .collect();
        self.cursor = match self.cursor {
            Some(_) if self.order.is_empty() => None,
            Some(p) => Some(p.min(self.order.len() - 1)),
            None => None,
        };
        self.history.retain(|&p| p < self.order.len());
    }

    /// Assert the documented invariants. Compiles away in release builds.
    #[inline]
    fn debug_check(&self) {
        if !cfg!(debug_assertions) {
            return;
        }
        assert_eq!(
            self.order.len(),
            self.tracks.len(),
            "play order must cover every entry"
        );
        assert!(
            self.cursor.is_none_or(|p| p < self.order.len()),
            "cursor {:?} is beyond {} position(s)",
            self.cursor,
            self.order.len()
        );
        assert!(
            self.history.iter().all(|&p| p < self.order.len()),
            "history holds a stale position"
        );
        let mut seen = vec![false; self.tracks.len()];
        for &i in &self.order {
            assert!(i < seen.len(), "play order index {i} out of bounds");
            assert!(!seen[i], "play order repeats index {i}");
            seen[i] = true;
        }
    }
}

impl From<Vec<Track>> for Playlist {
    fn from(tracks: Vec<Track>) -> Self {
        Self::from_tracks(tracks)
    }
}

impl FromIterator<Track> for Playlist {
    fn from_iter<I: IntoIterator<Item = Track>>(iter: I) -> Self {
        let tracks: Vec<Track> = iter.into_iter().collect();
        let order = (0..tracks.len()).collect();
        let seed = system_seed();
        Self {
            name: "playlist".into(),
            tracks,
            order,
            cursor: None,
            history: VecDeque::new(),
            mode: PlayMode::default(),
            rng: Rng::new(seed),
            seed,
        }
    }
}

impl Extend<Track> for Playlist {
    fn extend<I: IntoIterator<Item = Track>>(&mut self, iter: I) {
        for track in iter {
            self.push(track);
        }
    }
}

impl IntoIterator for Playlist {
    type Item = Track;
    type IntoIter = std::vec::IntoIter<Track>;

    fn into_iter(self) -> Self::IntoIter {
        self.tracks.into_iter()
    }
}

impl<'a> IntoIterator for &'a Playlist {
    type Item = &'a Track;
    type IntoIter = std::slice::Iter<'a, Track>;

    fn into_iter(self) -> Self::IntoIter {
        self.tracks.iter()
    }
}

impl fmt::Display for Playlist {
    /// Render the listing: a status line plus one row per entry in user order,
    /// marked `>` at the cursor. Column widths are computed from the content;
    /// the marker is two ASCII cells so nothing depends on wide-glyph handling.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (known, unknown) = self.duration_known();
        write!(
            f,
            "{} \u{b7} {} \u{b7} {}",
            self.name,
            if self.tracks.len() == 1 {
                "1 track".into()
            } else {
                format!("{} tracks", self.tracks.len())
            },
            self.mode.label()
        )?;
        if !self.tracks.is_empty() {
            f.write_str(&format!(" \u{b7} {}", format_duration(known)))?;
            if unknown > 0 {
                write!(f, " +{unknown}?")?;
            }
        }
        writeln!(f)?;
        if self.tracks.is_empty() {
            return writeln!(f, "(empty)");
        }

        let rows: Vec<(usize, String, String, Option<usize>)> = self
            .tracks
            .iter()
            .enumerate()
            .map(|(index, track)| {
                (
                    index,
                    track.display_line(),
                    track.duration_label(),
                    self.position_of(index),
                )
            })
            .collect();
        let index_width = self.tracks.len().to_string().len();
        let label_width = rows
            .iter()
            .map(|(_, l, _, _)| l.chars().count())
            .max()
            .unwrap_or(0);
        let duration_width = rows
            .iter()
            .map(|(_, _, d, _)| d.chars().count())
            .max()
            .unwrap_or(5)
            .max("--:--".len());

        for (index, label, duration, position) in &rows {
            let marker = if self.current_index() == Some(*index) {
                "> "
            } else {
                "  "
            };
            write!(
                f,
                "{marker}{:>index_width$}. {:<label_width$}  {:>duration_width$}",
                index + 1,
                label,
                duration,
            )?;
            if self.mode.shuffle
                && let Some(p) = position
            {
                write!(f, "  #{p}")?;
            }
            writeln!(f)?;
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn track(title: &str, mins: u64) -> Track {
        Track::new(format!("music/{title}.flac"))
            .with_title(title)
            .with_duration(Duration::from_secs(mins * 60))
    }

    fn playlist(n: usize) -> Playlist {
        Playlist::from_tracks((0..n).map(|i| track(&format!("t{i}"), (i as u64) + 1)))
    }

    fn ids(pl: &Playlist) -> Vec<TrackId> {
        pl.tracks().iter().map(|t| t.id()).collect()
    }

    #[test]
    fn push_grows_user_order_and_play_order() {
        let mut pl = Playlist::new();
        assert!(pl.is_empty());
        assert_eq!(pl.current(), None);
        for i in 0..3 {
            pl.push(track(&format!("t{i}"), i as u64));
        }
        assert_eq!(pl.len(), 3);
        assert_eq!(pl.play_order(), &[0, 1, 2]);
        assert_eq!(pl.remaining(), 3, "unstarted playlist queues everything");
    }

    #[test]
    fn uri_fallback_title() {
        assert_eq!(Track::new("a/b/song.mp3").title(), "song");
        assert_eq!(Track::new("https://x/y/z.wav?q=1").title(), "z");
        assert_eq!(Track::new("https://host/radio/").title(), "radio");
        assert_eq!(Track::new("").title(), "unknown");
        assert_eq!(Track::new(".hidden").title(), ".hidden");
    }

    #[test]
    fn advance_walks_then_ends_without_repeat() {
        let mut pl = playlist(2);
        assert!(matches!(
            pl.advance(),
            Nav::Moved {
                position: 0,
                index: 0
            }
        ));
        assert!(matches!(
            pl.advance(),
            Nav::Moved {
                position: 1,
                index: 1
            }
        ));
        assert!(pl.is_finished());
        assert_eq!(pl.advance(), Nav::Ended);
        assert_eq!(
            pl.advance(),
            Nav::Ended,
            "ended is sticky until told otherwise"
        );
        assert_eq!(pl.remaining(), 0);
    }

    #[test]
    fn advance_resumes_after_new_entries_are_queued() {
        let mut pl = playlist(1);
        pl.advance();
        assert_eq!(pl.advance(), Nav::Ended);
        pl.push(track("late", 3));
        assert!(matches!(
            pl.advance(),
            Nav::Moved {
                position: 1,
                index: 1
            }
        ));
    }

    #[test]
    fn repeat_all_wraps() {
        let mut pl = playlist(3);
        pl.set_repeat(Repeat::All);
        pl.advance();
        pl.advance();
        pl.advance();
        assert!(matches!(pl.advance(), Nav::Wrapped { position: 0, .. }));
    }

    #[test]
    fn repeat_one_requeues() {
        let mut pl = playlist(3);
        pl.set_repeat(Repeat::One);
        assert!(matches!(pl.advance(), Nav::Moved { position: 0, .. }));
        assert!(matches!(
            pl.advance(),
            Nav::Same {
                position: 0,
                index: 0
            }
        ));
        assert_eq!(pl.peek_next().map(|t| t.title()), Some("t0"));
        pl.set_repeat(Repeat::Off);
        assert!(matches!(
            pl.advance(),
            Nav::Moved {
                position: 1,
                index: 1
            }
        ));
    }

    #[test]
    fn peek_next_respects_mode() {
        let mut pl = playlist(2);
        assert_eq!(pl.peek_next().map(|t| t.title()), Some("t0"));
        pl.jump_to(1).unwrap();
        assert_eq!(pl.peek_next(), None);
        pl.set_repeat(Repeat::All);
        assert_eq!(pl.peek_next().map(|t| t.title()), Some("t0"));
    }

    #[test]
    fn back_uses_history_then_position() {
        let mut pl = playlist(4);
        pl.advance();
        pl.advance();
        pl.jump_to(3).unwrap();
        assert_eq!(pl.current_index(), Some(3));
        assert!(matches!(
            pl.back(),
            Nav::Moved {
                position: 1,
                index: 1
            }
        ));
        assert!(matches!(
            pl.back(),
            Nav::Moved {
                position: 0,
                index: 0
            }
        ));
        assert!(matches!(pl.back(), Nav::Same { position: 0, .. }));
    }

    #[test]
    fn history_is_bounded() {
        let mut pl = playlist(200);
        for _ in 0..200 {
            pl.advance();
        }
        assert_eq!(pl.history_len(), HISTORY_LIMIT);
        assert_eq!(
            pl.back().position(),
            Some(198),
            "retraces the previous step"
        );
        assert_eq!(pl.back().position(), Some(197));
    }

    #[test]
    fn shuffle_order_is_a_permutation_and_reproducible() {
        let mut a = Playlist::seeded("a", 7);
        let mut b = Playlist::seeded("b", 7);
        for i in 0..12 {
            a.push(track(&format!("t{i}"), i as u64));
            b.push(track(&format!("t{i}"), i as u64));
        }
        a.set_shuffle(true);
        b.set_shuffle(true);
        let mut sorted = a.play_order().to_vec();
        sorted.sort_unstable();
        assert_eq!(sorted, (0..12).collect::<Vec<_>>());
        assert_ne!(
            a.play_order(),
            &(0..12).collect::<Vec<_>>(),
            "seed 7 should shuffle"
        );
        assert_eq!(a.play_order(), b.play_order(), "same seed, same order");
    }

    #[test]
    fn shuffle_reorders_play_order_not_user_order() {
        let mut pl = playlist(6);
        let before = ids(&pl);
        pl.set_shuffle(true);
        pl.advance();
        pl.advance();
        assert_eq!(ids(&pl), before, "user order untouched");
        assert_eq!(pl.play_order().len(), 6);
    }

    #[test]
    fn toggling_shuffle_keeps_the_current_track_and_position() {
        let mut pl = playlist(8);
        pl.set_shuffle_seed(42);
        pl.set_shuffle(true);
        pl.jump_to(3).unwrap();
        let position = pl.current_position();
        pl.set_shuffle(true); // no-op
        assert_eq!(pl.current_index(), Some(3));
        assert_eq!(pl.current_position(), position);
        pl.set_shuffle(false);
        assert_eq!(pl.current_index(), Some(3), "the same track keeps playing");
        assert_eq!(
            pl.current_position(),
            Some(3),
            "in user order the play position is the index"
        );
        assert_eq!(pl.play_order(), &(0..8).collect::<Vec<_>>());
        pl.set_shuffle(true);
        assert_eq!(pl.current_index(), Some(3));
        assert_eq!(
            pl.current_position(),
            Some(3),
            "re-shuffled around the cursor"
        );
        assert_ne!(pl.play_order(), &(0..8).collect::<Vec<_>>());
    }

    #[test]
    fn insert_shifts_both_orderings() {
        let mut pl = playlist(3);
        pl.advance();
        pl.advance();
        assert_eq!(pl.current_index(), Some(1));
        pl.insert(0, track("early", 1)).unwrap();
        assert_eq!(pl.play_order(), &[1, 2, 3, 0]);
        assert_eq!(pl.current_index(), Some(2), "cursor follows its track");
        assert_eq!(pl.current().unwrap().title(), "t1");
        assert_eq!(pl.get(4), None);
        assert_eq!(
            pl.insert(9, track("nope", 1)),
            Err(Error::IndexOutOfBounds { index: 9, len: 4 })
        );
    }

    #[test]
    fn insert_at_end_appends() {
        let mut pl = playlist(2);
        let at = pl.insert(2, track("tail", 1)).unwrap();
        assert_eq!(at, 2);
        assert_eq!(pl.play_order(), &[0, 1, 2]);
    }

    #[test]
    fn removing_ahead_keeps_the_playing_track() {
        let mut pl = playlist(4);
        let third = pl.get(2).unwrap().id();
        pl.jump_to(2).unwrap();
        pl.remove(0);
        assert_eq!(pl.current_index(), Some(1));
        assert_eq!(pl.current().unwrap().id(), third);
        assert_eq!(pl.play_order(), &[0, 1, 2]);
        assert_eq!(pl.tracks().len(), 3);
    }

    #[test]
    fn removing_the_current_position_falls_to_the_successor() {
        let mut pl = playlist(3);
        pl.advance();
        pl.advance();
        let successor = pl.get(2).unwrap().id();
        pl.remove(1);
        assert_eq!(pl.current_position(), Some(1));
        assert_eq!(pl.current_index(), Some(1));
        assert_eq!(pl.current().unwrap().id(), successor);
    }

    #[test]
    fn removing_the_last_entry_clamps_the_cursor() {
        let mut pl = playlist(2);
        pl.jump_to(1).unwrap();
        pl.remove(1);
        assert_eq!(pl.current_position(), Some(0));
        assert_eq!(pl.current_index(), Some(0));
        pl.remove(0);
        assert_eq!(pl.current(), None);
        assert_eq!(pl.advance(), Nav::Ended);
        assert!(pl.history_len() == 0);
    }

    #[test]
    fn remove_id_and_clear() {
        let mut pl = playlist(3);
        let id = pl.get(1).unwrap().id();
        let (index, removed) = pl.remove_id(id).unwrap();
        assert_eq!(index, 1);
        assert_eq!(removed.title(), "t1");
        assert_eq!(pl.remove_id(id), None);
        pl.clear();
        assert!(pl.is_empty() && pl.play_order().is_empty());
        assert_eq!(pl.mode(), PlayMode::default(), "mode survives a clear");
    }

    #[test]
    fn retain_prunes_order_and_history() {
        let mut pl = playlist(6);
        for _ in 0..4 {
            pl.advance();
        }
        let dropped = pl.retain(|t| t.title() != "t0");
        assert_eq!(dropped, 1);
        assert_eq!(pl.len(), 5);
        assert_eq!(pl.tracks()[0].title(), "t1");
        assert_eq!(pl.current().unwrap().title(), "t4");
        pl.retain(|_| true);
        assert_eq!(pl.len(), 5);
    }

    #[test]
    fn merge_moves_entries() {
        let mut a = playlist(2);
        let mut b = playlist(3);
        b.advance();
        let base = a.merge(&mut b).unwrap();
        assert_eq!(base, 2);
        assert_eq!(a.len(), 5);
        assert_eq!(a.play_order(), &[0, 1, 2, 3, 4]);
        assert!(b.is_empty());
        assert_eq!(a.merge(&mut b), None);
    }

    #[test]
    fn swap_and_move_and_sort() {
        let mut pl = playlist(3);
        pl.swap(0, 2).unwrap();
        assert_eq!(pl.tracks()[0].title(), "t2");
        assert_eq!(pl.tracks()[2].title(), "t0");
        assert_eq!(pl.position_of(2), Some(0));
        assert_eq!(
            pl.swap(0, 9),
            Err(Error::IndexOutOfBounds { index: 9, len: 3 })
        );

        let mut pl = playlist(4);
        pl.jump_to(0).unwrap();
        pl.move_track(0, 3).unwrap();
        assert_eq!(pl.tracks()[3].title(), "t0");
        assert_eq!(pl.current_index(), Some(3), "follows the moved track");
        assert_eq!(pl.play_order(), &[3, 0, 1, 2]);
        assert_eq!(pl.move_track(0, 0), Ok(()));

        let mut pl = playlist(3);
        pl.move_track(2, 0).unwrap();
        pl.sort_by(|a, b| a.title().cmp(b.title()));
        assert_eq!(
            pl.tracks().iter().map(|t| t.title()).collect::<Vec<_>>(),
            ["t0", "t1", "t2"]
        );
    }

    #[test]
    fn reorder_validates_and_remaps() {
        let mut pl = playlist(3);
        pl.jump_to(0).unwrap();
        assert_eq!(pl.reorder(&[0, 1]), Err(Error::BadLayout { len: 3 }));
        assert_eq!(pl.reorder(&[0, 0, 2]), Err(Error::BadLayout { len: 3 }));
        assert_eq!(pl.reorder(&[0, 1, 9]), Err(Error::BadLayout { len: 3 }));
        pl.reorder(&[2, 0, 1]).unwrap();
        assert_eq!(pl.tracks()[0].title(), "t2");
        assert_eq!(pl.current().unwrap().title(), "t0");
        assert_eq!(pl.current_index(), Some(1));
        assert_eq!(pl.position_of(1), Some(0), "entry 1 now plays first");
    }

    #[test]
    fn search_ranks_and_finds() {
        let mut pl = Playlist::from_tracks([
            Track::new("one.mp3")
                .with_title("Midnight City")
                .with_artist("M83")
                .with_duration(Duration::from_secs(200)),
            Track::new("two.mp3")
                .with_title("City Lights")
                .with_artist("Nobody"),
            Track::new("three.mp3")
                .with_title("Other")
                .with_album("Midnight Album"),
        ]);
        assert_eq!(pl.search("midnight"), vec![0, 2]);
        assert_eq!(pl.search("city"), vec![1, 0], "prefix beats substring");
        assert_eq!(pl.search("midnight album"), vec![2], "album text matches");
        assert_eq!(pl.search("M83"), vec![0]);
        assert_eq!(pl.search(""), (0..3).collect::<Vec<_>>());
        assert_eq!(pl.find(|t| t.album().is_some()), Some(2));
        assert_eq!(pl.artists(), ["M83", "Nobody"]);
        pl.set_repeat(Repeat::One);
        assert_eq!(pl.repeat(), Repeat::One);
    }

    #[test]
    fn durations_are_partial_when_metadata_is_missing() {
        let mut pl = Playlist::from_tracks([
            track("a", 2),
            Track::new("b.mp3").with_duration(Duration::from_secs(90)),
            Track::new("c.mp3"),
        ]);
        let (known, unknown) = pl.duration_known();
        assert_eq!(known, Duration::from_secs(210));
        assert_eq!(unknown, 1);
        assert!(pl.get(2).unwrap().is_open_ended());
        assert_eq!(pl.duration_total(), None);
        pl.set_shuffle(true);
        let shuffled = pl.mode().label();
        assert_eq!(shuffled, "shuffle");
    }

    #[test]
    fn set_mode_and_labels() {
        let mut pl = playlist(3);
        pl.advance();
        assert!(
            pl.set_mode(PlayMode::SHUFFLE_ALL),
            "sequential -> shuffle all"
        );
        assert!(pl.is_shuffled());
        assert_eq!(pl.repeat(), Repeat::All);
        assert!(!pl.set_mode(PlayMode::SHUFFLE_ALL), "already in that mode");
        assert_eq!(
            pl.current().map(|t| t.title()),
            Some("t0"),
            "mode changes keep the cursor"
        );
        assert_eq!(pl.mode().label(), "shuffle \u{b7} repeat all");
        assert_eq!(PlayMode::SEQUENTIAL.label(), "in order");
        assert_eq!(PlayMode::repeating(Repeat::One).label(), "repeat one");
        assert!(!pl.toggle_shuffle());
        assert!(!pl.is_shuffled());
    }

    #[test]
    fn first_last_and_jump_helpers() {
        let mut pl = playlist(4);
        assert!(matches!(
            pl.last(),
            Some(Nav::Moved {
                position: 3,
                index: 3
            })
        ));
        assert!(matches!(
            pl.first(),
            Some(Nav::Moved {
                position: 0,
                index: 0
            })
        ));
        let id = pl.get(2).unwrap().id();
        assert!(pl.jump_to_id(id).unwrap().played());
        assert_eq!(pl.current_index(), Some(2));
        assert_eq!(
            pl.jump_to(99),
            Err(Error::IndexOutOfBounds { index: 99, len: 4 })
        );
        assert_eq!(
            pl.jump_to_id(TrackId::from_raw(9_999)),
            Err(Error::NotFound {
                id: TrackId::from_raw(9_999)
            })
        );
        assert!(matches!(
            pl.jump_to_position(2),
            Ok(Nav::Same {
                position: 2,
                index: 2
            })
        ));
        assert_eq!(
            pl.jump_to_position(4),
            Err(Error::IndexOutOfBounds { index: 4, len: 4 })
        );
        pl.reset();
        assert_eq!(pl.current(), None);
        assert_eq!(pl.history_len(), 0);
    }

    #[test]
    fn reset_then_advance_restarts_from_the_top() {
        let mut pl = playlist(3);
        pl.jump_to(2).unwrap();
        pl.reset();
        assert!(matches!(
            pl.advance(),
            Nav::Moved {
                position: 0,
                index: 0
            }
        ));
    }

    #[test]
    fn empty_playlist_navigation() {
        let mut pl = Playlist::new();
        assert_eq!(pl.advance(), Nav::Ended);
        assert_eq!(pl.back(), Nav::Ended);
        assert_eq!(pl.first(), None);
        assert_eq!(pl.last(), None);
        assert_eq!(pl.peek_next(), None);
        assert!(!pl.is_finished());
        assert_eq!(pl.duration_total(), None);
        assert_eq!(
            pl.jump_to(0),
            Err(Error::IndexOutOfBounds { index: 0, len: 0 })
        );
        assert_eq!(pl.set_repeat(Repeat::Off), Repeat::Off);
    }

    #[test]
    fn display_marks_the_cursor_and_sizes_columns() {
        let mut pl = Playlist::named("late night");
        pl.push(
            Track::new("a.mp3")
                .with_title("A Long Enough Title")
                .with_artist("Someone")
                .with_duration(Duration::from_secs(3715)),
        );
        pl.push(Track::new("b.mp3").with_title("Short"));
        pl.advance();
        pl.advance();
        let text = pl.to_string();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(
            lines[0],
            "late night \u{b7} 2 tracks \u{b7} in order \u{b7} 1:01:55 +1?"
        );
        assert!(lines[1].starts_with("  1. "), "{:?}", lines[1]);
        assert!(lines[2].starts_with("> 2. "), "{:?}", lines[2]);
        assert!(lines[2].ends_with("--:--"), "{:?}", lines[2]);
        let columns: Vec<usize> = lines[1..3].iter().map(|l| l.chars().count()).collect();
        assert_eq!(columns[0], columns[1], "rows align: {columns:?}");
        assert_eq!(format_duration(Duration::from_secs(200)), "03:20");

        let mut single = playlist(1);
        single.set_shuffle(true);
        assert!(single.to_string().contains("#0"), "{single}");
        let empty = Playlist::named("nothing");
        assert_eq!(
            empty.to_string(),
            "nothing \u{b7} 0 tracks \u{b7} in order\n(empty)\n"
        );
    }

    #[test]
    fn iterators_and_extending() {
        let mut pl: Playlist = (0..3).map(|i| track(&format!("t{i}"), i as u64)).collect();
        pl.extend([track("t3", 4), track("t4", 5)]);
        assert_eq!(pl.len(), 5);
        assert_eq!(pl.iter().count(), 5);
        assert_eq!(pl.iter_playback().count(), 5);
        assert_eq!(pl.iter_playback().next().unwrap().title(), "t0");
        assert_eq!((&pl).into_iter().count(), 5);
        let owned: Vec<Track> = pl.clone().into_iter().collect();
        assert_eq!(owned.len(), 5);
        assert_eq!(ids(&pl), owned.iter().map(|t| t.id()).collect::<Vec<_>>());
    }

    #[test]
    fn hydration_and_positional_lookups() {
        let mut pl = playlist(3);
        let id = pl.get(1).unwrap().id();
        assert!(
            !pl.iter().any(Track::is_open_ended),
            "the helper gives every entry a length"
        );
        assert_eq!(TrackId::from_raw(id.as_raw()), id);

        pl.jump_to(1).unwrap();
        let (position, index) = (pl.current_position().unwrap(), pl.current_index().unwrap());
        assert_eq!(index, 1);
        assert_eq!(pl.index_at(position), Some(index));
        assert_eq!(pl.position_of(index), Some(position));

        // Late metadata lands without disturbing the cursor or the play order.
        let order = pl.play_order().to_vec();
        pl.get_mut(index).unwrap().set_artist("Someone");
        for t in pl.tracks_mut() {
            if t.duration().is_none() {
                t.set_duration(Duration::from_secs(60));
            }
        }
        assert_eq!(pl.get(1).unwrap().artist(), Some("Someone"));
        assert_eq!(pl.duration_total(), Some(Duration::from_secs(360)));
        assert_eq!(pl.play_order(), order);
        assert_eq!(pl.current_index(), Some(index));
        assert_eq!(pl.get(1).unwrap().display_line(), "Someone \u{2014} t1");
        assert_eq!(
            pl.get(1).unwrap().relevance("T1"),
            Some(0),
            "exact, case-insensitive"
        );

        // A shuffled pass visits every index exactly once.
        pl.reshuffle();
        pl.set_shuffle(true);
        let mut visited = pl.play_order().to_vec();
        visited.sort_unstable();
        assert_eq!(visited, vec![0, 1, 2]);
        assert!(pl.shuffle_seed() != 0);
    }

    #[test]
    fn random_operations_preserve_invariants() {
        // Fuzz-ish: interleave edits and navigation, then check nothing is lost.
        let mut pl = Playlist::seeded("fuzz", 1234);
        let mut expected: Vec<TrackId> = Vec::new();
        for i in 0..40 {
            pl.push(track(&format!("t{i}"), i as u64));
            expected.push(pl.tracks().last().unwrap().id());
        }
        for round in 0..300u64 {
            let r = mix(round ^ pl.shuffle_seed());
            match r % 7 {
                0 => {
                    pl.advance();
                }
                1 => {
                    pl.back();
                }
                2 => {
                    let at = (r as usize) % (pl.len() + 1);
                    let id = TrackId::next();
                    let t = Track::new(format!("x{id}.mp3")).with_id(id);
                    if pl.insert(at, t).is_ok() {
                        expected.insert(at, id);
                    }
                }
                3 => {
                    if !pl.is_empty() {
                        let at = (r as usize) % pl.len();
                        if pl.remove(at).is_some() {
                            expected.remove(at);
                        }
                    }
                }
                4 => {
                    if pl.len() > 1 {
                        let a = (r as usize) % pl.len();
                        let b = (r as usize / 3) % pl.len();
                        let _ = pl.swap(a, b);
                        expected.swap(a, b);
                    }
                }
                5 => {
                    if pl.len() > 1 {
                        let a = (r as usize) % pl.len();
                        let b = (r as usize / 5) % pl.len();
                        let moved = expected.remove(a);
                        expected.insert(b, moved);
                        let _ = pl.move_track(a, b);
                    }
                }
                _ => {
                    pl.set_shuffle((r & 1) == 1);
                }
            }
            pl.debug_check();
            assert_eq!(ids(&pl), expected, "round {round}");
            let mut sorted = pl.play_order().to_vec();
            sorted.sort_unstable();
            assert_eq!(sorted, (0..pl.len()).collect::<Vec<_>>(), "round {round}");
        }
        assert_eq!(pl.len(), expected.len());
        for _ in 0..64 {
            pl.advance();
        }
        pl.debug_check();
        assert!(pl.back().played(), "history survives the edits");
        assert!(!pl.is_empty());
        assert_eq!(
            pl.search("t").len(),
            pl.tracks()
                .iter()
                .filter(|t| t.title().starts_with('t'))
                .count()
        );
    }
}
