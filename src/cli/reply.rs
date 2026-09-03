//! Command replies as data, separated from how they are rendered.
//!
//! [`Output`] is the data a command reports; `Display` is the current text
//! rendering of it. A future JSON rendering can serialize the same types
//! without touching the command logic that builds them.

use std::fmt;
use std::time::Duration;

use bo::engine::State;
use bo::track::FadeShape;

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
    pub(crate) from: Duration,
    pub(crate) to: Duration,
}

/// The four shapes of `set`.
#[derive(Debug)]
pub(crate) enum SetResult {
    Master { v: f32 },
    TrackVolume { i: usize, v: f32 },
    TrackMuted { i: usize, muted: bool },
    TrackName { i: usize, name: String },
    ClipGain { track: usize, id: u64, gain: f32 },
    ClipFadeIn { track: usize, id: u64, d: Duration },
    ClipFadeOut { track: usize, id: u64, d: Duration },
    ClipFadeShape { track: usize, id: u64, shape: FadeShape },
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
    pub(crate) end: Duration,
}

/// The `ls` report.
#[derive(Debug)]
pub(crate) struct Ls {
    pub(crate) state: State,
    pub(crate) playhead: Duration,
    pub(crate) end: Duration,
    pub(crate) backend: &'static str,
    pub(crate) volume: f32,
    pub(crate) tracks: Vec<LsTrack>,
}

#[derive(Debug)]
pub(crate) struct LsTrack {
    pub(crate) name: Option<String>,
    pub(crate) volume: f32,
    pub(crate) muted: bool,
    pub(crate) end: Duration,
    pub(crate) clips: Vec<LsClip>,
}

#[derive(Debug)]
pub(crate) struct LsClip {
    pub(crate) id: u64,
    pub(crate) uri: String,
    pub(crate) at: Duration,
    pub(crate) end: Duration,
    pub(crate) from: Duration,
    pub(crate) src_to: Duration,
    pub(crate) gain: f32,
    pub(crate) fade_in: Duration,
    pub(crate) fade_out: Duration,
    pub(crate) fade_shape: FadeShape,
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
                    writeln!(
                        f,
                        "ok: track {track} clip #{} {} @ {} src={}-{}",
                        c.id,
                        c.uri,
                        Tc(c.at),
                        Tc(c.from),
                        Tc(c.to)
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
                writeln!(
                    f,
                    "session: {tracks} tracks | {clips} clips | ends {} | backend {backend}",
                    Tc(*end)
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
                SetResult::ClipGain { track, id, gain } => {
                    writeln!(f, "clip {track}#{id} gain {}", Gain(*gain))
                }
                SetResult::ClipFadeIn { track, id, d } => {
                    writeln!(f, "clip {track}#{id} fade_in {}", Tc(*d))
                }
                SetResult::ClipFadeOut { track, id, d } => {
                    writeln!(f, "clip {track}#{id} fade_out {}", Tc(*d))
                }
                SetResult::ClipFadeShape { track, id, shape } => {
                    writeln!(f, "clip {track}#{id} fade_shape {shape}")
                }
            },
            Self::Removed { track, id, uri } => {
                writeln!(f, "removed track {track} clip #{id} {uri}")
            }
            Self::Rendered {
                file,
                duration,
                stats,
            } => {
                match file {
                    Some(file) => writeln!(f, "rendered {file} ({})", Tc(*duration))?,
                    None => writeln!(f, "measure: {}", Tc(*duration))?,
                }
                if let Some(m) = stats {
                    let db = |v: f32| format!("{v:.1} dBFS");
                    writeln!(f, "peak: {}", db(m.peak_db))?;
                    writeln!(f, "true_peak: {}", db(m.true_peak_db))?;
                    writeln!(f, "rms: {}", db(m.rms_db))?;
                    let mut lufs_line = |name: &str, v: Option<f32>, unit: &str| match v {
                        Some(v) => writeln!(f, "{name}: {v:.1} {unit}"),
                        None => Ok(()),
                    };
                    lufs_line("integrated", m.integrated_lufs, "LUFS")?;
                    lufs_line("momentary_max", m.momentary_max_lufs, "LUFS")?;
                    lufs_line("short_term_max", m.short_term_max_lufs, "LUFS")?;
                    lufs_line("lra", m.lra, "LU")?;
                    if let Some(t) = m.loudest_1s {
                        writeln!(f, "loudest_1s: {}", Tc(t))?;
                    }
                    if let Some(t) = m.quietest_1s {
                        writeln!(f, "quietest_1s: {}", Tc(t))?;
                    }
                    if m.integrated_lufs.is_none()
                        && m.span < std::time::Duration::from_secs(3)
                    {
                        writeln!(f, "note: span under 3s, too short for LUFS")?;
                    }
                }
                Ok(())
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
                writeln!(
                    f,
                    "state: {}\nplayhead: {}\nend: {}\nbackend: {}\nvolume: {}\ntracks: {}",
                    ls.state,
                    Tc(ls.playhead),
                    Tc(ls.end),
                    ls.backend,
                    Gain(ls.volume),
                    ls.tracks.len()
                )?;
                for (ti, t) in ls.tracks.iter().enumerate() {
                    let mute = if t.muted { " muted" } else { "" };
                    let name = match &t.name {
                        Some(name) => format!("name={name} "),
                        None => String::new(),
                    };
                    writeln!(
                        f,
                        "track {ti}: {name}volume={}{mute} clips={} end={}",
                        Gain(t.volume),
                        t.clips.len(),
                        Tc(t.end)
                    )?;
                    for c in &t.clips {
                        let mut line = format!(
                            "  clip {}: uri={} at={} end={} src={}-{}",
                            c.id,
                            c.uri,
                            Tc(c.at),
                            Tc(c.end),
                            Tc(c.from),
                            Tc(c.src_to)
                        );
                        if c.gain != 1.0 {
                            line.push_str(&format!(" gain={}", Gain(c.gain)));
                        }
                        if c.fade_in > Duration::ZERO {
                            line.push_str(&format!(" fade_in={}", Tc(c.fade_in)));
                        }
                        if c.fade_out > Duration::ZERO {
                            line.push_str(&format!(" fade_out={}", Tc(c.fade_out)));
                        }
                        if c.fade_shape != FadeShape::Linear {
                            line.push_str(&format!(" fade_shape={}", c.fade_shape));
                        }
                        writeln!(f, "{line}")?;
                    }
                }
                Ok(())
            }
            Self::At { at, active } => {
                if active.is_empty() {
                    return writeln!(f, "silent at {}", Tc(*at));
                }
                for line in active {
                    writeln!(
                        f,
                        "track {}: clip={} uri={} at={} end={}",
                        line.track,
                        line.id,
                        line.uri,
                        Tc(line.at),
                        Tc(line.end)
                    )?;
                }
                Ok(())
            }
            Self::Text(text) => f.write_str(text),
        }
    }
}
