//! Command replies as data, separated from how they are rendered.
//!
//! [`Output`] is the data a command reports; the `Display` impl renders it as
//! the reply grammar every consumer (human or agent) reads:
//!
//! * every reply opens with `ok: ...` or `err: ...` — the status line;
//! * timecodes are `HH:MM:SS.fff` strings, gains two decimals;
//! * absent means default, except in `ls`, which dumps everything;
//! * an edit that a running graph could not take is worth one `note:` line;
//!   one that landed, or one made while nothing was playing, is not;
//! * a clip is one signature line: `clip #{id} '{uri}' {from}-{to} @ {at}`
//!   with `key=value` suffixes for non-default gain and fades;
//! * a track block is a header line with indented clip lines.
//!
//! `bo <command> --help` documents each command's reply shape; both render
//! from this module so they cannot drift apart.

use std::fmt::Write as _;
use std::fmt;
use std::time::Duration;

use bo::engine::rodio::{Probing, SourceLength};
use bo::engine::{Landed, State};
use bo::track::FadeShape;

/// A timecode, rendered as `HH:MM:SS.fff`. Exact integer math, no floats.
#[derive(Debug)]
pub(crate) struct Tc(pub(crate) Duration);

impl fmt::Display for Tc {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&bo::time::format(self.0))
    }
}

/// A gain or level, rendered with two decimals.
#[derive(Debug)]
pub(crate) struct Gain(pub(crate) f32);

impl fmt::Display for Gain {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:.2}", self.0)
    }
}

/// Single-quote a free string (uri, name, file) for a reply line. Every
/// string of that kind is quoted on output, so boundaries never depend on
/// guessing; an embedded quote is escaped.
pub(crate) fn quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "\\'"))
}

/// `""` for one, `"s"` otherwise.
fn plural(n: usize) -> &'static str {
    if n == 1 { "" } else { "s" }
}

/// The `note:` line for an edit a running graph could not take.
///
/// `None` says nothing: an edit that landed live needs no report, and one
/// made while nothing was playing lands at the next `play`, which is the
/// default rather than news.
fn pending_note(f: &mut fmt::Formatter<'_>, landed: Option<Landed>) -> fmt::Result {
    match landed {
        Some(Landed::Pending) => writeln!(f, "note: lands at next apply"),
        _ => Ok(()),
    }
}

/// The head of a clip signature: `clip #{id} '{uri}' {from}-{to} @ {at}`.
fn clip_head(id: u64, uri: &str, from: Duration, to: Duration, at: Duration) -> String {
    format!(
        "clip #{id} {} {}-{} @ {}",
        quote(uri),
        Tc(from),
        Tc(to),
        Tc(at)
    )
}

/// ` key=value` suffixes for a clip's non-default gain, placement and fades.
fn clip_suffix(
    gain: f32,
    pan: Option<f32>,
    fade_in: Duration,
    fade_in_from: f32,
    fade_out: Duration,
    fade_out_to: f32,
    shape: FadeShape,
) -> String {
    let mut s = String::new();
    if gain != 1.0 {
        s.push_str(&format!(" gain={}", Gain(gain)));
    }
    if let Some(p) = pan {
        s.push_str(&format!(" pan={}", Gain(p)));
    }
    if fade_in > Duration::ZERO {
        s.push_str(&format!(" fade_in={}", Tc(fade_in)));
    }
    if fade_in_from != 0.0 {
        s.push_str(&format!(" fade_in_from={}", Gain(fade_in_from)));
    }
    if fade_out > Duration::ZERO {
        s.push_str(&format!(" fade_out={}", Tc(fade_out)));
    }
    if fade_out_to != 0.0 {
        s.push_str(&format!(" fade_out_to={}", Gain(fade_out_to)));
    }
    if shape != FadeShape::Linear {
        s.push_str(&format!(" fade_shape={shape}"));
    }
    s
}

/// One clip reported by `put`, with the gain and fades it was placed with
/// (echoed only when non-default).
#[derive(Debug)]
pub(crate) struct PlacedClip {
    pub(crate) id: u64,
    pub(crate) uri: String,
    pub(crate) at: Duration,
    pub(crate) from: Duration,
    pub(crate) to: Duration,
    pub(crate) gain: f32,
    pub(crate) fade_in: Duration,
    pub(crate) fade_in_from: f32,
    pub(crate) fade_out: Duration,
    pub(crate) fade_out_to: f32,
    pub(crate) fade_shape: FadeShape,
}

/// Where a `route` landed, when it was a group bus: enough to name the
/// destination and say how many tracks share it.
#[derive(Debug)]
pub(crate) struct RoutedBus {
    pub(crate) id: u64,
    pub(crate) name: Option<String>,
    /// Tracks routed into this bus, after the move.
    pub(crate) tracks: usize,
}

/// The four shapes of `set`.
#[derive(Debug)]
pub(crate) enum SetResult {
    Master { v: f32 },
    TrackVolume { i: usize, v: f32 },
    TrackPan { i: usize, v: f32 },
    TrackMuted { i: usize, muted: bool },
    TrackName { i: usize, name: String },
    BusVolume { id: u64, v: f32 },
    BusMuted { id: u64, muted: bool },
    BusName { id: u64, name: String },
    ClipGain { track: usize, id: u64, gain: f32 },
    ClipPan {
        track: usize,
        id: u64,
        pan: Option<f32>,
    },
    ClipFadeIn { track: usize, id: u64, d: Duration },
    ClipFadeInFrom { track: usize, id: u64, level: f32 },
    ClipFadeOut { track: usize, id: u64, d: Duration },
    ClipFadeOutTo { track: usize, id: u64, level: f32 },
    ClipFadeShape { track: usize, id: u64, shape: FadeShape },
    ClipPanControl { track: usize, id: u64, control: String },
    ClipGainControl { track: usize, id: u64, control: String },
}

/// One source measured by `probe` without a uri.
#[derive(Debug)]
pub(crate) struct ProbeResult {
    pub(crate) uri: String,
    pub(crate) outcome: Result<Probing, String>,
}

/// One clip audible at `at <t>`.
#[derive(Debug)]
pub(crate) struct AtLine {
    pub(crate) track: usize,
    pub(crate) id: u64,
    pub(crate) uri: String,
    pub(crate) at: Duration,
    pub(crate) from: Duration,
    pub(crate) to: Duration,
}

/// The `ls` report.
#[derive(Debug)]
pub(crate) struct Ls {
    pub(crate) state: State,
    pub(crate) playhead: Duration,
    pub(crate) end: Duration,
    pub(crate) backend: &'static str,
    pub(crate) volume: f32,
    /// `BO_IDLE_TIMEOUT` seconds; a quiet, non-playing daemon exits after
    /// this long without a command (`0` disables). Shown so a long session
    /// cannot silently time out.
    pub(crate) idle_timeout: u64,
    /// Edits a running graph could not take, waiting for the next `apply`.
    pub(crate) pending: usize,
    /// The group buses (shown to users as plain buses), each with the number
    /// of tracks routed into it.
    pub(crate) buses: Vec<LsBus>,
    pub(crate) tracks: Vec<LsTrack>,
}

/// One group bus as `ls` shows it: its strip, and how many tracks share it.
#[derive(Debug)]
pub(crate) struct LsBus {
    pub(crate) id: u64,
    pub(crate) name: Option<String>,
    pub(crate) volume: f32,
    pub(crate) muted: bool,
    /// Tracks routed into this bus.
    pub(crate) tracks: usize,
}

/// Where a track's output points when it feeds a group bus: enough to name
/// the bus on the track's own line.
#[derive(Debug)]
pub(crate) struct BusLabel {
    pub(crate) id: u64,
    pub(crate) name: Option<String>,
}

#[derive(Debug)]
pub(crate) struct LsTrack {
    pub(crate) name: Option<String>,
    pub(crate) volume: f32,
    pub(crate) pan: f32,
    pub(crate) muted: bool,
    /// The group bus this track feeds, when it does not feed the master.
    pub(crate) bus: Option<BusLabel>,
    pub(crate) end: Duration,
    pub(crate) clips: Vec<LsClip>,
}

#[derive(Debug)]
pub(crate) struct LsClip {
    pub(crate) id: u64,
    pub(crate) uri: String,
    pub(crate) at: Duration,
    pub(crate) from: Duration,
    pub(crate) to: Duration,
    pub(crate) gain: f32,
    /// The clip's own placement, when it does not follow its track.
    pub(crate) pan: Option<f32>,
    /// Its pan control sources as one text, when any are plugged in.
    pub(crate) pan_control: Option<String>,
    /// Its gain control sources as one text, when any are plugged in.
    pub(crate) gain_control: Option<String>,
    pub(crate) fade_in: Duration,
    pub(crate) fade_in_from: f32,
    pub(crate) fade_out: Duration,
    pub(crate) fade_out_to: f32,
    pub(crate) fade_shape: FadeShape,
}

/// What `apply` reports: which edits landed, and whether a graph had to be
/// rebuilt for the rest.
#[derive(Debug)]
pub(crate) struct ApplyReport {
    /// Edits the running graph took as they were.
    pub(crate) live: usize,
    /// The timecode a rebuild started from, when one was needed.
    pub(crate) rebuilt: Option<Duration>,
    /// Nothing is playing, so there is no graph to land anything on.
    pub(crate) stopped: bool,
}

/// The data of one command reply.
#[derive(Debug)]
pub(crate) enum Output {
    Put {
        track: usize,
        clips: Vec<PlacedClip>,
        landed: Option<Landed>,
    },
    Session {
        tracks: usize,
        clips: usize,
        end: Duration,
        backend: &'static str,
        playhead: Duration,
        note: Option<String>,
    },
    Paused {
        at: Duration,
    },
    Resumed {
        at: Duration,
    },
    Stopped,
    Seeked {
        at: Duration,
    },
    Applied(ApplyReport),
    Set {
        result: SetResult,
        landed: Option<Landed>,
    },
    Removed {
        track: usize,
        id: u64,
        uri: String,
        at: Duration,
        from: Duration,
        to: Duration,
        gain: f32,
        fade_in: Duration,
        fade_in_from: f32,
        fade_out: Duration,
        fade_out_to: f32,
        fade_shape: FadeShape,
        landed: Option<Landed>,
    },
    Moved {
        from_track: usize,
        to_track: usize,
        clip: PlacedClip,
        landed: Option<Landed>,
    },
    Routed {
        track: usize,
        /// Where the track's output now points; `None` is the master.
        to: Option<RoutedBus>,
        landed: Option<Landed>,
    },
    Rendered {
        file: Option<String>,
        duration: Duration,
        stats: Option<bo::engine::measure::Measurement>,
    },
    Saved {
        file: String,
    },
    Loaded {
        file: String,
    },
    Reset {
        tracks: usize,
    },
    Check {
        clips: usize,
        problems: Vec<(String, String)>,
        /// Distinct sources whose length the container could not state.
        estimated: usize,
    },
    Probed {
        uri: String,
        length: SourceLength,
        /// Interleaved channels of the source, as decoded.
        channels: u16,
    },
    ProbedMany {
        sources: Vec<ProbeResult>,
    },
    Ls(Ls),
    At {
        at: Duration,
        active: Vec<AtLine>,
    },
    /// Pre-rendered text (the grouped help).
    Text(&'static str),
}

impl fmt::Display for Output {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Put {
                track,
                clips,
                landed,
            } => {
                writeln!(
                    f,
                    "ok: {} clip{} on track {track}",
                    clips.len(),
                    plural(clips.len())
                )?;
                for c in clips {
                    writeln!(
                        f,
                        "  {}{}",
                        clip_head(c.id, &c.uri, c.from, c.to, c.at),
                        clip_suffix(
                            c.gain,
                            None,
                            c.fade_in,
                            c.fade_in_from,
                            c.fade_out,
                            c.fade_out_to,
                            c.fade_shape,
                        )
                    )?;
                }
                pending_note(f, *landed)
            }
            Self::Session {
                tracks,
                clips,
                end,
                backend,
                playhead,
                note,
            } => {
                writeln!(
                    f,
                    "ok: {tracks} track{}, {clips} clip{}, ends {}, playing from {}",
                    plural(*tracks),
                    plural(*clips),
                    Tc(*end),
                    Tc(*playhead)
                )?;
                if let Some(note) = note {
                    writeln!(f, "note: {note}, '{backend}' backend")?;
                }
                Ok(())
            }
            Self::Paused { at } => writeln!(f, "ok: paused at {}", Tc(*at)),
            Self::Resumed { at } => writeln!(f, "ok: playing from {}", Tc(*at)),
            Self::Stopped => f.write_str("ok: stopped\n"),
            Self::Seeked { at } => writeln!(f, "ok: playhead at {}", Tc(*at)),
            Self::Applied(report) => {
                if report.stopped {
                    return f.write_str("ok: not playing; changes land at next play\n");
                }
                match report.rebuilt {
                    // A rebuild is the one outcome worth a timecode: it says
                    // where the sound picked back up.
                    Some(at) => {
                        writeln!(f, "ok: rebuilt from {}", Tc(at))?;
                        if report.live > 0 {
                            writeln!(
                                f,
                                "note: {} change{} landed live",
                                report.live,
                                plural(report.live)
                            )?;
                        }
                    }
                    None if report.live == 0 => f.write_str("ok: nothing pending\n")?,
                    None => writeln!(
                        f,
                        "ok: {} change{} landed",
                        report.live,
                        plural(report.live)
                    )?,
                }
                Ok(())
            }
            Self::Set { result: set, landed } => {
                let (var, value): (String, String) = match set {
                    SetResult::Master { v } => ("master".into(), Gain(*v).to_string()),
                    SetResult::TrackVolume { i, v } => {
                        (format!("track.{i}.volume"), Gain(*v).to_string())
                    }
                    SetResult::TrackPan { i, v } => {
                        (format!("track.{i}.pan"), Gain(*v).to_string())
                    }
                    SetResult::TrackMuted { i, muted } => {
                        (format!("track.{i}.muted"), muted.to_string())
                    }
                    SetResult::TrackName { i, name } => (format!("track.{i}.name"), name.clone()),
                    SetResult::BusVolume { id, v } => (format!("bus.{id}.volume"), Gain(*v).to_string()),
                    SetResult::BusMuted { id, muted } => (format!("bus.{id}.muted"), muted.to_string()),
                    SetResult::BusName { id, name } => (format!("bus.{id}.name"), name.clone()),
                    SetResult::ClipGain { track, id, gain } => {
                        (format!("clip.{track}.{id}.gain"), Gain(*gain).to_string())
                    }
                    SetResult::ClipPan { track, id, pan } => (
                        format!("clip.{track}.{id}.pan"),
                        match pan {
                            Some(v) => Gain(*v).to_string(),
                            None => "auto".to_string(),
                        },
                    ),
                    SetResult::ClipFadeIn { track, id, d } => {
                        (format!("clip.{track}.{id}.fade_in"), Tc(*d).to_string())
                    }
                    SetResult::ClipFadeInFrom { track, id, level } => (
                        format!("clip.{track}.{id}.fade_in_from"),
                        Gain(*level).to_string(),
                    ),
                    SetResult::ClipFadeOut { track, id, d } => {
                        (format!("clip.{track}.{id}.fade_out"), Tc(*d).to_string())
                    }
                    SetResult::ClipFadeOutTo { track, id, level } => (
                        format!("clip.{track}.{id}.fade_out_to"),
                        Gain(*level).to_string(),
                    ),
                    SetResult::ClipFadeShape { track, id, shape } => {
                        (format!("clip.{track}.{id}.fade_shape"), shape.to_string())
                    }
                    SetResult::ClipPanControl { track, id, control } => {
                        (format!("clip.{track}.{id}.pan_control"), control.clone())
                    }
                    SetResult::ClipGainControl { track, id, control } => {
                        (format!("clip.{track}.{id}.gain_control"), control.clone())
                    }
                };
                writeln!(f, "ok: `{var}` set to `{value}`")?;
                pending_note(f, *landed)
            }
            Self::Removed {
                track,
                id,
                uri,
                at,
                from,
                to,
                gain,
                fade_in,
                fade_in_from,
                fade_out,
                fade_out_to,
                fade_shape,
                landed,
            } => {
                writeln!(f, "ok: removed 1 clip from track {track}")?;
                writeln!(
                    f,
                    "  {}{}",
                    clip_head(*id, uri, *from, *to, *at),
                    clip_suffix(
                        *gain,
                        None,
                        *fade_in,
                        *fade_in_from,
                        *fade_out,
                        *fade_out_to,
                        *fade_shape,
                    )
                )?;
                pending_note(f, *landed)
            }
            Self::Moved {
                from_track,
                to_track,
                clip,
                landed,
            } => {
                writeln!(
                    f,
                    "ok: moved 1 clip from track {from_track} to track {to_track} @ {}",
                    Tc(clip.at)
                )?;
                writeln!(
                    f,
                    "  {}{}",
                    clip_head(clip.id, &clip.uri, clip.from, clip.to, clip.at),
                    clip_suffix(
                        clip.gain,
                        None,
                        clip.fade_in,
                        clip.fade_in_from,
                        clip.fade_out,
                        clip.fade_out_to,
                        clip.fade_shape,
                    )
                )?;
                pending_note(f, *landed)
            }
            Self::Routed { track, to, landed } => {
                match to {
                    None => writeln!(f, "ok: track {track} routed to master")?,
                    Some(bus) => {
                        let name = match &bus.name {
                            Some(name) => quote(name),
                            None => "untitled".to_string(),
                        };
                        writeln!(
                            f,
                            "ok: track {track} routed to bus #{} {name} ({} track{})",
                            bus.id,
                            bus.tracks,
                            plural(bus.tracks)
                        )?;
                    }
                }
                pending_note(f, *landed)
            }
            Self::Rendered {
                file,
                duration,
                stats,
            } => {
                match file {
                    Some(file) => writeln!(f, "ok: rendered {} ({})", quote(file), Tc(*duration))?,
                    None => writeln!(f, "ok: measured {}", Tc(*duration))?,
                }
                if let Some(m) = stats {
                    writeln!(f, "measure:")?;
                    writeln!(f, "  peak_db={:.1}", m.peak_db)?;
                    writeln!(f, "  true_peak_db={:.1}", m.true_peak_db)?;
                    writeln!(f, "  rms_db={:.1}", m.rms_db)?;
                    let mut lufs_line =
                        |name: &str, v: Option<f32>| -> fmt::Result {
                            if let Some(v) = v {
                                writeln!(f, "  {name}={v:.1}")?;
                            }
                            Ok(())
                        };
                    lufs_line("integrated_lufs", m.integrated_lufs)?;
                    lufs_line("momentary_max_lufs", m.momentary_max_lufs)?;
                    lufs_line("short_term_max_lufs", m.short_term_max_lufs)?;
                    lufs_line("lra", m.lra)?;
                    if let Some(t) = m.loudest_1s {
                        writeln!(f, "  loudest_1s={}", Tc(t))?;
                    }
                    if let Some(t) = m.quietest_1s {
                        writeln!(f, "  quietest_1s={}", Tc(t))?;
                    }
                    if m.integrated_lufs.is_none() && m.span < Duration::from_secs(3) {
                        writeln!(f, "note: span under 3s, too short for LUFS")?;
                    }
                }
                Ok(())
            }
            Self::Saved { file } => writeln!(f, "ok: saved {}", quote(file)),
            Self::Loaded { file } => writeln!(f, "ok: loaded {}", quote(file)),
            Self::Reset { tracks } => {
                writeln!(f, "ok: {tracks} track{} removed", plural(*tracks))
            }
            Self::Check {
                clips,
                problems,
                estimated,
            } => {
                if problems.is_empty() {
                    writeln!(f, "ok: {clips} clip{}, all sources ok", plural(*clips))?;
                } else {
                    writeln!(
                        f,
                        "err: {} problem{}",
                        problems.len(),
                        plural(problems.len())
                    )?;
                    for (uri, err) in problems {
                        writeln!(f, "  {} error={err}", quote(uri))?;
                    }
                }
                // An estimated length is not a problem — every clip carries
                // its own finite out-point — but it is worth one note.
                if *estimated > 0 {
                    writeln!(
                        f,
                        "note: {} source{} with no header length; lengths were estimated",
                        estimated,
                        plural(*estimated)
                    )?;
                }
                Ok(())
            }
            Self::Probed {
                uri,
                length,
                channels,
            } => match length {
                SourceLength::Exact(d) => {
                    writeln!(f, "ok: {} duration={} channels={}", quote(uri), Tc(*d), channels)
                }
                SourceLength::Estimated(d) => {
                    writeln!(
                        f,
                        "ok: {} duration={} estimated channels={}",
                        quote(uri),
                        Tc(*d),
                        channels
                    )
                }
            },
            Self::ProbedMany { sources } => {
                if sources.is_empty() {
                    return f.write_str("ok: no sources in the arrangement\n");
                }
                let unreadable = sources.iter().filter(|s| s.outcome.is_err()).count();
                if unreadable == 0 {
                    writeln!(
                        f,
                        "ok: {} source{}",
                        sources.len(),
                        plural(sources.len())
                    )?;
                } else {
                    writeln!(
                        f,
                        "err: {} sources, {unreadable} unreadable",
                        sources.len()
                    )?;
                }
                for s in sources {
                    match &s.outcome {
                        Ok(Probing {
                            length: SourceLength::Exact(d),
                            channels,
                        }) => writeln!(
                            f,
                            "  {} duration={} channels={}",
                            quote(&s.uri),
                            Tc(*d),
                            channels
                        )?,
                        Ok(Probing {
                            length: SourceLength::Estimated(d),
                            channels,
                        }) => writeln!(
                            f,
                            "  {} duration={} estimated channels={}",
                            quote(&s.uri),
                            Tc(*d),
                            channels
                        )?,
                        Err(e) => writeln!(f, "  {} error={e}", quote(&s.uri))?,
                    }
                }
                Ok(())
            }
            Self::Ls(ls) => {
                let clips: usize = ls.tracks.iter().map(|t| t.clips.len()).sum();
                writeln!(
                    f,
                    "ok: {} track{}, {} clip{}",
                    ls.tracks.len(),
                    plural(ls.tracks.len()),
                    clips,
                    plural(clips)
                )?;
                // Edits waiting for a graph are the one thing `ls` reports
                // that is not arrangement data: they are what an `apply`
                // would have to do something about.
                let pending = match ls.pending {
                    0 => String::new(),
                    n => format!(" pending={n}"),
                };
                writeln!(
                    f,
                    "{}, playhead at {}, '{}' backend, end={}, master={}, idle_timeout={}{}",
                    ls.state,
                    Tc(ls.playhead),
                    ls.backend,
                    Tc(ls.end),
                    Gain(ls.volume),
                    Tc(Duration::from_secs(ls.idle_timeout)),
                    pending
                )?;
                for bus in &ls.buses {
                    let name = match &bus.name {
                        Some(name) => quote(name),
                        None => "untitled".to_string(),
                    };
                    let muted = if bus.muted { " muted" } else { "" };
                    writeln!(
                        f,
                        "bus #{} {name} vol={} tracks={}{}",
                        bus.id,
                        Gain(bus.volume),
                        bus.tracks,
                        muted
                    )?;
                }
                for (ti, t) in ls.tracks.iter().enumerate() {
                    let name = match &t.name {
                        Some(name) => quote(name),
                        None => "untitled".to_string(),
                    };
                    write!(
                        f,
                        "track {ti} {name} vol={} pan={} end={}",
                        Gain(t.volume),
                        Gain(t.pan),
                        Tc(t.end)
                    )?;
                    if t.muted {
                        write!(f, " muted")?;
                    }
                    if let Some(bus) = &t.bus {
                        write!(f, " bus=#{}", bus.id)?;
                        if let Some(name) = &bus.name {
                            write!(f, " {}", quote(name))?;
                        }
                    }
                    writeln!(f)?;
                    for c in &t.clips {
                        let mut suffix = clip_suffix(
                            c.gain,
                            c.pan,
                            c.fade_in,
                            c.fade_in_from,
                            c.fade_out,
                            c.fade_out_to,
                            c.fade_shape,
                        );
                        if let Some(control) = &c.pan_control {
                            let _ = write!(suffix, " pan_control={control}");
                        }
                        if let Some(control) = &c.gain_control {
                            let _ = write!(suffix, " gain_control={control}");
                        }
                        writeln!(
                            f,
                            "  {}{}",
                            clip_head(c.id, &c.uri, c.from, c.to, c.at),
                            suffix
                        )?;
                    }
                }
                Ok(())
            }
            Self::At { at, active } => {
                if active.is_empty() {
                    return writeln!(f, "ok: silent at {}", Tc(*at));
                }
                writeln!(
                    f,
                    "ok: {} clip{} at {}",
                    active.len(),
                    plural(active.len()),
                    Tc(*at)
                )?;
                for line in active {
                    writeln!(
                        f,
                        "  track {}: {}",
                        line.track,
                        clip_head(line.id, &line.uri, line.from, line.to, line.at)
                    )?;
                }
                Ok(())
            }
            Self::Text(text) => f.write_str(text),
        }
    }
}

/// A real rendered reply for one command, used by `bo help <command>`.
/// The example goes through the same [`Display`] the daemon uses, so the
/// documented shape can never drift from the actual output.
pub(crate) fn example_reply(command: &str) -> Option<String> {
    use bo::engine::measure::Measurement;
    use bo::track::FadeShape::Linear;
    use std::time::Duration as D;

    let s = D::from_secs;
    let ms = D::from_millis;
    let clip = |id: u64, uri: &str, at: D, from: D, to: D, gain: f32| PlacedClip {
        id,
        uri: uri.to_string(),
        at,
        from,
        to,
        gain,
        fade_in: D::ZERO,
        fade_in_from: 0.0,
        fade_out: D::ZERO,
        fade_out_to: 0.0,
        fade_shape: Linear,
    };
    let out = match command {
        "put" => Output::Put {
            track: 0,
            clips: vec![clip(0, "/srv/bed.wav", D::ZERO, D::ZERO, s(30), 1.0)],
            landed: None,
        },
        "take" => Output::Removed {
            track: 0,
            id: 1,
            uri: "/srv/voice.wav".into(),
            at: s(10),
            from: D::ZERO,
            to: s(5),
            gain: 1.0,
            fade_in: D::ZERO,
            fade_in_from: 0.0,
            fade_out: D::ZERO,
            fade_out_to: 0.0,
            fade_shape: Linear,
            landed: None,
        },
        "move" => Output::Moved {
            from_track: 0,
            to_track: 1,
            clip: clip(3, "/srv/voice.wav", s(5), D::ZERO, s(10), 0.5),
            landed: None,
        },
        "route" => Output::Routed {
            track: 0,
            to: Some(RoutedBus {
                id: 0,
                name: Some("music".into()),
                tracks: 2,
            }),
            landed: None,
        },
        "ls" => Output::Ls(Ls {
            state: State::Stopped,
            playhead: D::ZERO,
            end: s(30),
            backend: "silent",
            volume: 1.0,
            idle_timeout: 600,
            pending: 0,
            buses: vec![LsBus {
                id: 0,
                name: Some("music".into()),
                volume: 0.7,
                muted: false,
                tracks: 1,
            }],
            tracks: vec![
                LsTrack {
                    name: Some("bed".into()),
                    volume: 0.4,
                    pan: 0.0,
                    muted: false,
                    bus: Some(BusLabel {
                        id: 0,
                        name: Some("music".into()),
                    }),
                    end: s(30),
                    clips: vec![LsClip {
                        id: 0,
                        uri: "/srv/bed.wav".into(),
                        at: D::ZERO,
                        from: D::ZERO,
                        to: s(30),
                        gain: 0.5,
                        pan: None,
                        fade_in: ms(600),
                        fade_in_from: 0.0,
                        fade_out: D::ZERO,
                        fade_out_to: 0.0,
                        fade_shape: Linear,
                        pan_control: None,
                        gain_control: None,
                    }],
                },
                LsTrack {
                    name: Some("voice".into()),
                    volume: 0.8,
                    pan: 0.0,
                    muted: true,
                    bus: None,
                    end: s(8),
                    clips: vec![LsClip {
                        id: 0,
                        uri: "/srv/voice.wav".into(),
                        at: D::ZERO,
                        from: D::ZERO,
                        to: s(8),
                        gain: 1.0,
                        pan: None,
                        fade_in: D::ZERO,
                        fade_in_from: 0.0,
                        fade_out: D::ZERO,
                        fade_out_to: 0.0,
                        fade_shape: Linear,
                        pan_control: None,
                        gain_control: None,
                    }],
                },
            ],
        }),
        "at" => Output::At {
            at: s(5),
            active: vec![AtLine {
                track: 0,
                id: 0,
                uri: "/srv/bed.wav".into(),
                at: D::ZERO,
                from: D::ZERO,
                to: s(30),
            }],
        },
        "render" => Output::Rendered {
            file: Some("/srv/mix.wav".into()),
            duration: s(30),
            stats: Some(Measurement {
                span: s(30),
                peak_db: -6.0,
                true_peak_db: -5.8,
                rms_db: -9.0,
                integrated_lufs: Some(-12.3),
                momentary_max_lufs: Some(-9.1),
                short_term_max_lufs: Some(-11.2),
                lra: Some(3.1),
                loudest_1s: Some(s(21)),
                quietest_1s: Some(s(3)),
            }),
        },
        "check" => Output::Check {
            clips: 2,
            problems: Vec::new(),
            // A header-less source (an mp3 without a Xing/Info frame) is a
            // note, not a problem: clips carry their own out-points.
            estimated: 1,
        },
        "probe" => Output::Probed {
            uri: "/srv/bed.wav".into(),
            // Containers that state no length (a vbr mp3 without a Xing/Info
            // frame) are decoded to their end and marked as estimated.
            length: SourceLength::Estimated(s(30)),
            channels: 2,
        },
        "set" => Output::Set {
            result: SetResult::TrackVolume { i: 0, v: 0.4 },
            landed: None,
        },
        "play" => Output::Session {
            tracks: 2,
            clips: 2,
            end: s(30),
            backend: "silent",
            playhead: D::ZERO,
            note: None,
        },
        "seek" => Output::Seeked { at: s(4) },
        "pause" => Output::Paused { at: s(12) },
        "resume" => Output::Resumed { at: s(12) },
        "stop" => Output::Stopped,
        "apply" => Output::Applied(ApplyReport {
            live: 2,
            rebuilt: None,
            stopped: false,
        }),
        "save" => Output::Saved {
            file: "/srv/mix.bo".into(),
        },
        "load" => Output::Loaded {
            file: "/srv/mix.bo".into(),
        },
        "reset" => Output::Reset { tracks: 2 },
        _ => return None,
    };
    Some(out.to_string())
}
