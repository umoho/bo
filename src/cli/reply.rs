//! Command replies as data, separated from how they are rendered.
//!
//! [`Output`] is the data a command reports; the `Display` impl renders it as
//! the reply grammar every consumer (human or agent) reads:
//!
//! * every reply opens with `ok: ...` or `err: ...` — the status line;
//! * timecodes are `HH:MM:SS.fff` strings, gains two decimals;
//! * absent means default, except in `ls`, which dumps everything;
//! * a clip is one signature line: `clip #{id} '{uri}' {from}-{to} @ {at}`
//!   with `key=value` suffixes for non-default gain and fades;
//! * a track block is a header line with indented clip lines.
//!
//! `bo <command> --help` documents each command's reply shape; both render
//! from this module so they cannot drift apart.

use std::fmt;
use std::time::Duration;

use bo::engine::rodio::SourceLength;
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

/// ` key=value` suffixes for a clip's non-default gain and fades.
fn clip_suffix(
    gain: f32,
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

/// The four shapes of `set`.
#[derive(Debug)]
pub(crate) enum SetResult {
    Master { v: f32 },
    TrackVolume { i: usize, v: f32 },
    TrackMuted { i: usize, muted: bool },
    TrackName { i: usize, name: String },
    ClipGain { track: usize, id: u64, gain: f32 },
    ClipFadeIn { track: usize, id: u64, d: Duration },
    ClipFadeInFrom { track: usize, id: u64, level: f32 },
    ClipFadeOut { track: usize, id: u64, d: Duration },
    ClipFadeOutTo { track: usize, id: u64, level: f32 },
    ClipFadeShape { track: usize, id: u64, shape: FadeShape },
}

/// One source measured by `probe` without a uri.
#[derive(Debug)]
pub(crate) struct ProbeResult {
    pub(crate) uri: String,
    pub(crate) outcome: Result<SourceLength, String>,
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
    pub(crate) from: Duration,
    pub(crate) to: Duration,
    pub(crate) gain: f32,
    pub(crate) fade_in: Duration,
    pub(crate) fade_in_from: f32,
    pub(crate) fade_out: Duration,
    pub(crate) fade_out_to: f32,
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
        at: Duration,
        from: Duration,
        to: Duration,
        gain: f32,
        fade_in: Duration,
        fade_in_from: f32,
        fade_out: Duration,
        fade_out_to: f32,
        fade_shape: FadeShape,
    },
    Moved {
        from_track: usize,
        to_track: usize,
        clip: PlacedClip,
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
                            c.fade_in,
                            c.fade_in_from,
                            c.fade_out,
                            c.fade_out_to,
                            c.fade_shape,
                        )
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
            Self::Applied { rebuilt } => match rebuilt {
                Some(at) => writeln!(f, "ok: rebuilt from {}", Tc(*at)),
                None => f.write_str("ok: not playing; changes land at next play\n"),
            },
            Self::Set(set) => {
                let (var, value): (String, String) = match set {
                    SetResult::Master { v } => ("master".into(), Gain(*v).to_string()),
                    SetResult::TrackVolume { i, v } => {
                        (format!("track.{i}.volume"), Gain(*v).to_string())
                    }
                    SetResult::TrackMuted { i, muted } => {
                        (format!("track.{i}.muted"), muted.to_string())
                    }
                    SetResult::TrackName { i, name } => (format!("track.{i}.name"), name.clone()),
                    SetResult::ClipGain { track, id, gain } => {
                        (format!("clip.{track}.{id}.gain"), Gain(*gain).to_string())
                    }
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
                };
                writeln!(f, "ok: `{var}` set to `{value}`")
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
            } => {
                writeln!(f, "ok: removed 1 clip from track {track}")?;
                writeln!(
                    f,
                    "  {}{}",
                    clip_head(*id, uri, *from, *to, *at),
                    clip_suffix(
                        *gain,
                        *fade_in,
                        *fade_in_from,
                        *fade_out,
                        *fade_out_to,
                        *fade_shape,
                    )
                )
            }
            Self::Moved {
                from_track,
                to_track,
                clip,
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
                        clip.fade_in,
                        clip.fade_in_from,
                        clip.fade_out,
                        clip.fade_out_to,
                        clip.fade_shape,
                    )
                )
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
            Self::Probed { uri, length } => match length {
                SourceLength::Exact(d) => {
                    writeln!(f, "ok: {} duration={}", quote(uri), Tc(*d))
                }
                SourceLength::Estimated(d) => {
                    writeln!(f, "ok: {} duration={} estimated", quote(uri), Tc(*d))
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
                        Ok(SourceLength::Exact(d)) => {
                            writeln!(f, "  {} duration={}", quote(&s.uri), Tc(*d))?
                        }
                        Ok(SourceLength::Estimated(d)) => {
                            writeln!(f, "  {} duration={} estimated", quote(&s.uri), Tc(*d))?
                        }
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
                writeln!(
                    f,
                    "{}, playhead at {}, '{}' backend, end={}, master={}, idle_timeout={}",
                    ls.state,
                    Tc(ls.playhead),
                    ls.backend,
                    Tc(ls.end),
                    Gain(ls.volume),
                    Tc(Duration::from_secs(ls.idle_timeout))
                )?;
                for (ti, t) in ls.tracks.iter().enumerate() {
                    let name = match &t.name {
                        Some(name) => quote(name),
                        None => "untitled".to_string(),
                    };
                    let muted = if t.muted { " muted" } else { "" };
                    writeln!(
                        f,
                        "track {ti} {name} vol={} end={}{}",
                        Gain(t.volume),
                        Tc(t.end),
                        muted
                    )?;
                    for c in &t.clips {
                        writeln!(
                            f,
                            "  {}{}",
                            clip_head(c.id, &c.uri, c.from, c.to, c.at),
                            clip_suffix(
                                c.gain,
                                c.fade_in,
                                c.fade_in_from,
                                c.fade_out,
                                c.fade_out_to,
                                c.fade_shape,
                            )
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
        },
        "move" => Output::Moved {
            from_track: 0,
            to_track: 1,
            clip: clip(3, "/srv/voice.wav", s(5), D::ZERO, s(10), 0.5),
        },
        "ls" => Output::Ls(Ls {
            state: State::Stopped,
            playhead: D::ZERO,
            end: s(30),
            backend: "silent",
            volume: 1.0,
            idle_timeout: 600,
            tracks: vec![
                LsTrack {
                    name: Some("bed".into()),
                    volume: 0.4,
                    muted: false,
                    end: s(30),
                    clips: vec![LsClip {
                        id: 0,
                        uri: "/srv/bed.wav".into(),
                        at: D::ZERO,
                        from: D::ZERO,
                        to: s(30),
                        gain: 0.5,
                        fade_in: ms(600),
                        fade_in_from: 0.0,
                        fade_out: D::ZERO,
                        fade_out_to: 0.0,
                        fade_shape: Linear,
                    }],
                },
                LsTrack {
                    name: Some("voice".into()),
                    volume: 0.8,
                    muted: true,
                    end: s(8),
                    clips: vec![LsClip {
                        id: 0,
                        uri: "/srv/voice.wav".into(),
                        at: D::ZERO,
                        from: D::ZERO,
                        to: s(8),
                        gain: 1.0,
                        fade_in: D::ZERO,
                        fade_in_from: 0.0,
                        fade_out: D::ZERO,
                        fade_out_to: 0.0,
                        fade_shape: Linear,
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
        },
        "set" => Output::Set(SetResult::TrackVolume { i: 0, v: 0.4 }),
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
        "apply" => Output::Applied {
            rebuilt: Some(s(4)),
        },
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
