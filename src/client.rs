//! The client: [`Bo`], a typed handle on a [`session::Session`], forwarding
//! commands to the session's executor.
//!
//! Today the executor is the daemon on its Unix socket — the same
//! arrangement every `bo` invocation shares. Each method is one command,
//! sent as typed JSON and decoded back into typed results; the command
//! vocabulary itself ([`Slice`], [`TrackPos`], [`Put`], [`Error`], …) lives
//! in `bo_core::command`, shared with the executors that run it.
//!
//! # Placing a clip
//!
//! [`Bo::put`] takes three plain things: a source address (a file the caller
//! manages — bo does not open or probe it unless it must), a [`Slice`] (the
//! `from..to` window into that source, or the whole source), and a
//! [`TrackPos`] (a track and a timecode). A clip with an open end (a slice
//! whose `to` is `None`) is measured where the arrangement lives, because
//! only decoding the source can say where it ends; a closed slice touches no
//! disk at all until play or render.
//!
//! ```no_run
//! use bo::client::{Bo, Slice, TrackRef};
//! use std::time::Duration;
//!
//! let mut bo = Bo::new();   // the default session: the shared daemon
//! // The 1:00–2:00 window of the file, on track 0 at 30 s in:
//! let put = bo.put("bed.wav", "1:00-2:00".parse()?, TrackRef(0).at(Duration::from_secs(30)))?;
//! assert_eq!(put.track, 0);
//! # Ok::<(), bo::client::Error>(())
//! ```

use std::time::Duration;

use crate::session::Session;

// The command vocabulary, shared with the executors in bo-core and
// bo-engine; re-exported here so `bo::client::Slice` reads as before.
pub use bo_core::command::{Error, Landed, Overlap, PlacedClip, Put, Slice, TrackPos, TrackRef};

/// A typed client on a [`Session`]: the arrangement lives there, commands
/// travel there, and the replies come back typed.
///
/// [`Bo::new`] is the default session — the daemon on
/// `$TMPDIR/bo/daemon.sock`, spawned on demand — the same session the CLI
/// speaks to, so a program and a shell can work one arrangement. Any other
/// session (another socket, later a process) is [`Bo::with_session`].
#[derive(Debug)]
pub struct Bo {
    session: Session,
}

impl Default for Bo {
    fn default() -> Self {
        Self::new()
    }
}

impl Bo {
    /// A client on the default session: the daemon on
    /// `$TMPDIR/bo/daemon.sock`, spawned on demand.
    #[must_use]
    pub fn new() -> Self {
        Self::with_session(Session::default())
    }

    /// A client on a session of your own.
    #[must_use]
    pub fn with_session(session: Session) -> Self {
        Self { session }
    }

    /// Place a clip: the `from..to` window `slice` of source `uri`, on
    /// `on.track` at track-time `on.at`.
    ///
    /// A refused put — a slice with no end whose source cannot be measured,
    /// or a placement that collides with a resident clip — is an `Err` and
    /// leaves the session exactly as it was. The track is grown to fit. A
    /// clip placed past the end of a running track's queue joins that queue
    /// as it is placed; anything else waits for an `apply` — see
    /// [`Put::landed`].
    pub fn put(&mut self, uri: &str, slice: Slice, on: TrackPos) -> Result<Put, Error> {
        let request = serde_json::json!({
            "cmd": "put",
            "uri": uri,
            "from_ms": ms(slice.from),
            "to_ms": slice.to.map(ms),
            "track": on.track,
            "at_ms": ms(on.at),
        });
        let reply = self.session.request(&request).map_err(Error::Daemon)?;
        decode_put(&reply)
    }
}

/// A duration as whole milliseconds — the wire's exact unit.
fn ms(d: Duration) -> u64 {
    d.as_secs() * 1000 + u64::from(d.subsec_millis())
}

/// Decode a daemon reply to a put into a [`Put`] or a typed [`Error`].
fn decode_put(reply: &str) -> Result<Put, Error> {
    let v: serde_json::Value = serde_json::from_str(reply)
        .map_err(|e| Error::Daemon(format!("bad daemon reply: {e}")))?;
    if v.get("ok").and_then(serde_json::Value::as_bool) != Some(true) {
        return Err(decode_error(&v));
    }
    let get = |key: &str| {
        v.get(key)
            .and_then(serde_json::Value::as_u64)
            .ok_or_else(|| Error::Daemon(format!("bad daemon reply: missing {key}")))
    };
    let id = get("id")?;
    let from = Duration::from_millis(get("from_ms")?);
    let to = Duration::from_millis(get("to_ms")?);
    let at = Duration::from_millis(get("at_ms")?);
    let landed = match v.get("landed").and_then(serde_json::Value::as_str) {
        Some("live") => Landed::Live,
        Some("pending") => Landed::Pending,
        other => {
            return Err(Error::Daemon(format!(
                "bad daemon reply: landed {other:?}"
            )))
        }
    };
    Ok(Put {
        track: get("track")? as usize,
        clip: PlacedClip {
            id,
            uri: v
                .get("uri")
                .and_then(serde_json::Value::as_str)
                .ok_or_else(|| Error::Daemon("bad daemon reply: missing uri".to_string()))?
                .to_string(),
            at,
            from,
            to,
            gain: 1.0,
            fade: Default::default(),
        },
        landed,
    })
}

/// Decode the daemon's error object into the typed [`Error`].
fn decode_error(v: &serde_json::Value) -> Error {
    let error = v.get("error").cloned().unwrap_or(serde_json::Value::Null);
    let kind = error
        .get("kind")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("other");
    let message = |fallback: &str| {
        error
            .get("message")
            .and_then(serde_json::Value::as_str)
            .unwrap_or(fallback)
            .to_string()
    };
    let dur = |key: &str| {
        error
            .get(key)
            .and_then(serde_json::Value::as_u64)
            .map(Duration::from_millis)
    };
    match kind {
        "overlap" => Error::Overlap(Overlap {
            track: error
                .get("track")
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(0) as usize,
            at: dur("at_ms").unwrap_or_default(),
            conflict: error
                .get("conflict")
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(0),
            conflict_at: dur("conflict_at_ms").unwrap_or_default(),
            conflict_end: dur("conflict_end_ms").unwrap_or_default(),
            next_free: dur("next_free_ms").unwrap_or_default(),
        }),
        "probe" => Error::Probe {
            uri: error
                .get("uri")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("")
                .to_string(),
            why: message("cannot measure the source"),
        },
        "parse" => Error::Parse(message("bad request")),
        _ => Error::Daemon(message("the daemon refused the request")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decode_a_put_reply() {
        let reply = r#"{"ok":true,"track":2,"id":5,"uri":"bed.wav",
            "from_ms":60000,"to_ms":120000,"at_ms":30000,"landed":"pending"}"#;
        let put = decode_put(reply).unwrap();
        assert_eq!(put.track, 2);
        assert_eq!(put.clip.id, 5);
        assert_eq!(put.clip.from, Duration::from_secs(60));
        assert_eq!(put.clip.at, Duration::from_secs(30));
        assert_eq!(put.landed, Landed::Pending);
    }

    #[test]
    fn decode_an_overlap_reply_back_into_typed_error() {
        let reply = r#"{"ok":false,"error":{"kind":"overlap","track":0,"at_ms":0,
            "conflict":1,"conflict_at_ms":5000,"conflict_end_ms":10000,
            "next_free_ms":10000,"message":"put refused"}}"#;
        match decode_put(reply).unwrap_err() {
            Error::Overlap(overlap) => {
                assert_eq!(overlap.conflict, 1);
                assert_eq!(overlap.next_free, Duration::from_secs(10));
            }
            other => panic!("expected an overlap, got {other:?}"),
        }
    }

    #[test]
    fn ms_round_trips_through_milliseconds() {
        let d = Duration::from_secs_f64(61.234);
        assert_eq!(Duration::from_millis(ms(d)), d);
    }
}
