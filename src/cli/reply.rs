//! Command replies as data, separated from how they are rendered.
//!
//! [`Output`] is the data a command reports; `Display` is the current text
//! rendering of it. A future JSON rendering can serialize the same types
//! without touching the command logic that builds them.

use std::fmt;
use std::time::Duration;

use bo::engine::State;

/// A timecode, rendered as `HH:MM:SS.fff`. Exact integer math, no floats.
#[derive(Debug)]
pub(crate) struct Tc(pub(crate) Duration);

impl fmt::Display for Tc {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let d = self.0;
        let total_ms = d.as_secs().saturating_mul(1000) + u64::from(d.subsec_millis());
        let ms = total_ms % 1000;
        let s = (total_ms / 1000) % 60;
        let m = (total_ms / 60_000) % 60;
        let h = total_ms / 3_600_000;
        write!(f, "{h:02}:{m:02}:{s:02}.{ms:03}")
    }
}

/// A gain, rendered with two decimals.
#[derive(Debug)]
pub(crate) struct Gain(pub(crate) f32);

impl fmt::Display for Gain {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:.2}", self.0)
    }
}

/// One clip reported by `put`.
#[derive(Debug)]
pub(crate) struct PlacedClip {
    pub(crate) id: u64,
    pub(crate) uri: String,
    pub(crate) at: Duration,
    pub(crate) open_ended: bool,
}

/// The four shapes of `set`.
#[derive(Debug)]
pub(crate) enum SetResult {
    Master { v: f32 },
    TrackVolume { i: usize, v: f32 },
    TrackMuted { i: usize, muted: bool },
    TrackName { i: usize, name: String },
}

/// One source measured by `probe` without a uri.
#[derive(Debug)]
pub(crate) struct ProbeResult {
    pub(crate) uri: String,
    pub(crate) outcome: Result<Duration, String>,
}

/// One clip audible at `at <t>`.
#[derive(Debug)]
pub(crate) struct AtLine {
    pub(crate) track: usize,
    pub(crate) id: u64,
    pub(crate) uri: String,
    pub(crate) at: Duration,
    pub(crate) end: Option<Duration>,
}

/// The `ls` report.
#[derive(Debug)]
pub(crate) struct Ls {
    pub(crate) state: State,
    pub(crate) playhead: Duration,
    pub(crate) end: Option<Duration>,
    pub(crate) backend: &'static str,
    pub(crate) volume: f32,
    pub(crate) tracks: Vec<LsTrack>,
}

#[derive(Debug)]
pub(crate) struct LsTrack {
    pub(crate) name: Option<String>,
    pub(crate) volume: f32,
    pub(crate) muted: bool,
    pub(crate) end: Option<Duration>,
    pub(crate) clips: Vec<LsClip>,
}

#[derive(Debug)]
pub(crate) struct LsClip {
    pub(crate) id: u64,
    pub(crate) uri: String,
    pub(crate) at: Duration,
    pub(crate) end: Option<Duration>,
    pub(crate) from: Duration,
    pub(crate) src_to: Option<Duration>,
}

/// The data of one command reply.
#[derive(Debug)]
pub(crate) enum Output {
    Put {
        track: usize,
        clips: Vec<PlacedClip>,
    },
    Session {
        tracks: usize,
        clips: usize,
        end: Option<Duration>,
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
    Applied {
        rebuilt: Option<Duration>,
    },
    Set(SetResult),
    Removed {
        track: usize,
        id: u64,
        uri: String,
    },
    Rendered {
        file: String,
        duration: Duration,
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
        problems: Vec<String>,
    },
    Probed {
        uri: String,
        duration: Duration,
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
            Self::Put { track, clips } => {
                for c in clips {
                    let open = if c.open_ended { " (open-ended)" } else { "" };
                    writeln!(
                        f,
                        "ok: track {track} clip #{} {} @ {}{open}",
                        c.id,
                        c.uri,
                        Tc(c.at)
                    )?;
                }
                Ok(())
            }
            Self::Session {
                tracks,
                clips,
                end,
                backend,
                playhead,
                note,
            } => {
                let end = end
                    .map(|d| Tc(d).to_string())
                    .unwrap_or_else(|| "inf".into());
                writeln!(
                    f,
                    "session: {tracks} tracks | {clips} clips | ends {end} | backend {backend}"
                )?;
                writeln!(f, "playing from {}", Tc(*playhead))?;
                if let Some(note) = note {
                    writeln!(f, "({note})")?;
                }
                Ok(())
            }
            Self::Paused { at } => writeln!(f, "paused at {}", Tc(*at)),
            Self::Resumed { at } => writeln!(f, "playing from {}", Tc(*at)),
            Self::Stopped => f.write_str("stopped\n"),
            Self::Seeked { at } => writeln!(f, "playhead at {}", Tc(*at)),
            Self::Applied { rebuilt } => match rebuilt {
                Some(at) => writeln!(f, "apply: rebuilt from {}", Tc(*at)),
                None => f.write_str("apply: transport not playing; changes land at next play\n"),
            },
            Self::Set(set) => match set {
                SetResult::Master { v } => writeln!(f, "master {}", Gain(*v)),
                SetResult::TrackVolume { i, v } => writeln!(f, "track {i} volume {}", Gain(*v)),
                SetResult::TrackMuted { i, muted } => {
                    writeln!(f, "track {i} {}", if *muted { "muted" } else { "unmuted" })
                }
                SetResult::TrackName { i, name } => writeln!(f, "track {i} named {name:?}"),
            },
            Self::Removed { track, id, uri } => {
                writeln!(f, "removed track {track} clip #{id} {uri}")
            }
            Self::Rendered { file, duration } => {
                writeln!(f, "rendered {file} ({})", Tc(*duration))
            }
            Self::Saved { file } => writeln!(f, "saved {file}"),
            Self::Loaded { file } => writeln!(f, "loaded {file}"),
            Self::Reset { tracks } => {
                let noun = if *tracks == 1 { "track" } else { "tracks" };
                writeln!(f, "reset: {tracks} {noun} removed")
            }
            Self::Check { clips, problems } => {
                if problems.is_empty() {
                    writeln!(f, "check: {clips} clips, all sources ok")
                } else {
                    let noun = if problems.len() == 1 { "problem" } else { "problems" };
                    writeln!(f, "check: {} {noun}", problems.len())?;
                    for problem in problems {
                        writeln!(f, "  - {problem}")?;
                    }
                    Ok(())
                }
            }
            Self::Probed { uri, duration } => {
                writeln!(f, "probe: {uri} {} {:.2} s", Tc(*duration), duration.as_secs_f64())
            }
            Self::ProbedMany { sources } => {
                if sources.is_empty() {
                    return f.write_str("probe: no sources in the arrangement\n");
                }
                let noun = if sources.len() == 1 { "source" } else { "sources" };
                writeln!(f, "probe: {} {noun}", sources.len())?;
                for s in sources {
                    match &s.outcome {
                        Ok(d) => writeln!(f, "  {} {} {:.2} s", s.uri, Tc(*d), d.as_secs_f64())?,
                        Err(e) => writeln!(f, "  {}: {e}", s.uri)?,
                    }
                }
                Ok(())
            }
            Self::Ls(ls) => {
                let end = ls
                    .end
                    .map(|d| Tc(d).to_string())
                    .unwrap_or_else(|| "inf".into());
                writeln!(
                    f,
                    "state: {}\nplayhead: {}\nend: {end}\nbackend: {}\nvolume: {}\ntracks: {}",
                    ls.state,
                    Tc(ls.playhead),
                    ls.backend,
                    Gain(ls.volume),
                    ls.tracks.len()
                )?;
                for (ti, t) in ls.tracks.iter().enumerate() {
                    let dur = t
                        .end
                        .map(|d| Tc(d).to_string())
                        .unwrap_or_else(|| "inf".into());
                    let mute = if t.muted { " muted" } else { "" };
                    let name = match &t.name {
                        Some(name) => format!("name={name} "),
                        None => String::new(),
                    };
                    writeln!(
                        f,
                        "track {ti}: {name}volume={}{mute} clips={} end={dur}",
                        Gain(t.volume),
                        t.clips.len()
                    )?;
                    for c in &t.clips {
                        let end = c
                            .end
                            .map(|d| Tc(d).to_string())
                            .unwrap_or_else(|| "inf".into());
                        let src_to = c
                            .src_to
                            .map(|d| Tc(d).to_string())
                            .unwrap_or_else(|| "inf".into());
                        writeln!(
                            f,
                            "  clip {}: uri={} at={} end={end} src={}-{src_to}",
                            c.id,
                            c.uri,
                            Tc(c.at),
                            Tc(c.from)
                        )?;
                    }
                }
                Ok(())
            }
            Self::At { at, active } => {
                if active.is_empty() {
                    return writeln!(f, "silent at {}", Tc(*at));
                }
                for line in active {
                    let end = line
                        .end
                        .map(|d| Tc(d).to_string())
                        .unwrap_or_else(|| "inf".into());
                    writeln!(
                        f,
                        "track {}: clip={} uri={} at={} end={end}",
                        line.track,
                        line.id,
                        line.uri,
                        Tc(line.at)
                    )?;
                }
                Ok(())
            }
            Self::Text(text) => f.write_str(text),
        }
    }
}
