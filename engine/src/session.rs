//! A session: an arrangement and a transport, one object a host holds.
//!
//! The client never touches this — it holds a [`Connection`](crate::client)
//! to a session run elsewhere. A host (the daemon, an in-process embedder)
//! opens a [`Session`], runs commands through [`Session::exec`], and drives
//! its clock with the lifecycle methods. The audio backend is chosen at
//! open ([`Session::open`] honours `BO_BACKEND=silent`, falling back to
//! silence when no device opens); a deterministic headless session is
//! [`Session::silent`].

use std::time::Duration;

use bo_core::bus::Group;
use bo_core::command::{Command, Error, Outcome};
use bo_core::track::Track;

use crate::rodio::Rodio;
use crate::{Backend, BackendError, Change, Player, Silent, State};

/// The session's audio backend: real audio when the device opened, silence
/// otherwise. Forced silent by `BO_BACKEND=silent`, or when no device
/// exists (the `String` is why).
#[derive(Debug)]
pub enum Runtime {
    /// Headless.
    Silent(Silent, Option<String>),
    /// Real audio.
    Rodio(Rodio),
}

impl Runtime {
    /// The forced/test backend.
    #[must_use]
    pub fn silent() -> Self {
        Self::Silent(Silent::default(), None)
    }

    /// What a session should use at open: rodio unless `BO_BACKEND=silent`
    /// says otherwise, falling back to silence when no device can be opened.
    #[must_use]
    pub fn open() -> Self {
        if std::env::var("BO_BACKEND").as_deref() == Ok("silent") {
            return Self::silent();
        }
        match Rodio::try_new() {
            Ok(rodio) => Self::Rodio(rodio),
            Err(e) => Self::Silent(Silent::default(), Some(format!("no audio device: {e}"))),
        }
    }

    /// The backend's name, for a status line.
    #[must_use]
    pub fn name(&self) -> &'static str {
        match self {
            Self::Silent(..) => "silent",
            Self::Rodio(..) => "rodio",
        }
    }

    /// Why playback is silent, when it is a fallback rather than a choice.
    #[must_use]
    pub fn note(&self) -> Option<&str> {
        match self {
            Self::Silent(_, note) => note.as_deref(),
            Self::Rodio(..) => None,
        }
    }
}

impl Backend for Runtime {
    fn play(&mut self, tracks: &[Track], groups: &[Group], at: Duration) -> Result<(), BackendError> {
        match self {
            Self::Silent(backend, _) => backend.play(tracks, groups, at),
            Self::Rodio(backend) => backend.play(tracks, groups, at),
        }
    }

    fn pause(&mut self) {
        match self {
            Self::Silent(backend, _) => backend.pause(),
            Self::Rodio(backend) => backend.pause(),
        }
    }

    fn resume(&mut self) {
        match self {
            Self::Silent(backend, _) => backend.resume(),
            Self::Rodio(backend) => backend.resume(),
        }
    }

    fn stop(&mut self) {
        match self {
            Self::Silent(backend, _) => backend.stop(),
            Self::Rodio(backend) => backend.stop(),
        }
    }

    fn set_volume(&mut self, volume: f32) {
        match self {
            Self::Silent(backend, _) => backend.set_volume(volume),
            Self::Rodio(backend) => backend.set_volume(volume),
        }
    }

    fn land(&mut self, tracks: &[Track], at: Duration, change: &Change) -> bool {
        match self {
            Self::Silent(backend, _) => backend.land(tracks, at, change),
            Self::Rodio(backend) => backend.land(tracks, at, change),
        }
    }

    fn position(&self) -> Option<Duration> {
        match self {
            Self::Silent(backend, _) => backend.position(),
            Self::Rodio(backend) => backend.position(),
        }
    }
}

/// A session: an arrangement and a transport over a chosen [`Runtime`].
///
/// A host opens one ([`Session::open`]), runs every command through
/// [`Session::exec`], and drives the clock with [`Session::advance`] /
/// [`Session::is_finished`]. Commands are the only way in; the underlying
/// player is reached only through a transitional accessor until the text
/// surface runs through `exec` too.
#[derive(Debug)]
pub struct Session {
    player: Player<Runtime>,
}

impl Session {
    /// A deterministic headless session (tests, CI, embedders that make no
    /// sound).
    #[must_use]
    pub fn silent() -> Self {
        Self::with_runtime(Runtime::silent())
    }

    /// A session on the runtime the environment asks for: rodio unless
    /// `BO_BACKEND=silent`, falling back to silence without a device.
    #[must_use]
    pub fn open() -> Self {
        Self::with_runtime(Runtime::open())
    }

    /// A session on a specific runtime.
    #[must_use]
    pub fn with_runtime(runtime: Runtime) -> Self {
        Self {
            player: Player::new(runtime),
        }
    }

    /// Run one command. Everything a session can do goes through here.
    pub fn exec(&mut self, command: Command) -> Result<Outcome, Error> {
        crate::exec(&mut self.player, command)
    }

    // ---- host lifecycle: the clock a long-lived host drives ----

    /// Current transport state.
    #[must_use]
    pub fn state(&self) -> State {
        self.player.state()
    }

    /// The playhead timecode.
    #[must_use]
    pub fn playhead(&self) -> Duration {
        self.player.playhead()
    }

    /// The whole arrangement's length: the latest end across tracks.
    #[must_use]
    pub fn duration(&self) -> Duration {
        self.player.duration()
    }

    /// Step the playhead forward by wall-clock elapsed time.
    pub fn advance(&mut self, dt: Duration) {
        self.player.advance(dt);
    }

    /// Whether playback has run to the arrangement's end.
    #[must_use]
    pub fn is_finished(&self) -> bool {
        self.player.is_finished()
    }

    /// The audio backend's name, for a status line.
    #[must_use]
    pub fn backend_name(&self) -> &'static str {
        self.player.backend().name()
    }

    /// Why playback is silent, when it is a fallback rather than a choice.
    #[must_use]
    pub fn backend_note(&self) -> Option<&str> {
        self.player.backend().note()
    }

    /// The player, for the surfaces that still edit the arrangement
    /// directly. Transitional: as commands cover the surface this goes
    /// away.
    pub fn player_mut(&mut self) -> &mut Player<Runtime> {
        &mut self.player
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bo_core::command::{OnTrack, Outcome};

    #[test]
    fn a_silent_session_runs_commands() {
        let mut s = Session::silent();
        assert_eq!(s.backend_name(), "silent");
        assert_eq!(s.state(), State::Stopped);

        let Outcome::Inserted(put) = s
            .exec(Command::Insert {
                uri: "a.wav".to_string(),
                from: Duration::ZERO,
                to: Some(Duration::from_secs(10)),
                on: OnTrack::Track { index: 0, at: Duration::ZERO },
            })
            .unwrap()
        else {
            panic!("expected Inserted")
        };
        assert_eq!(put.clip.id, 0);
        assert_eq!(s.duration(), Duration::from_secs(10));

        let Outcome::Played(played) = s.exec(Command::Play).unwrap() else {
            panic!("expected Played")
        };
        assert_eq!(played.end, Duration::from_secs(10));
        assert_eq!(s.state(), State::Playing);

        // The silent backend cannot report a clock; the host advances it.
        s.advance(Duration::from_secs(9));
        assert!(!s.is_finished());
        s.advance(Duration::from_secs(2));
        assert!(s.is_finished(), "the playhead passed the end");

        s.exec(Command::Stop).unwrap();
        assert_eq!(s.state(), State::Stopped);
    }
}
