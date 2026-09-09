//! `pybo` — the Python face of `bo`, binding only the public `bo::client`
//! surface (the [`Bo`] struct and the vocabulary types its methods speak),
//! never the raw `Command` protocol or the engine.
//!
//! Every verb travels to the daemon over its Unix socket, exactly like the
//! Rust client does; the daemon binary is found by `bo` itself (PATH, or a
//! `target/{debug,release}/bo` walking up from the working directory), so a
//! Python host needs no extra setup. Several `Bo`s on one socket share one
//! arrangement — Python, the Rust client and the CLI are all faces of the
//! same session.
//!
//! Times are spoken in one dialect: a `Timecode` accepts seconds
//! (`1.23`), the lenient text forms (`SS`, `MM:SS`, `HH:MM:SS`, optional
//! `.fff`) — a bare number is seconds — and formats back as `HH:MM:SS.fff`.
//! Durations in dicts and the tree are whole milliseconds, the wire's unit.
//!
//! ```python
//! import pybo
//! bo = pybo.Bo()                       # the shared daemon on the default socket
//! r = bo.put("bed.wav", pybo.Track(0).at("0:00"))
//! bo.set("track.0.volume", 0.4)
//! bo.play()
//! ```

use std::time::Duration;

use bo::client as bc;
use pyo3::create_exception;
use pyo3::exceptions::{PyTypeError, PyValueError};
use pyo3::prelude::*;
use pyo3::types::{
    PyAnyMethods, PyBool, PyDict, PyDictMethods, PyFloat, PyInt, PyList, PyListMethods,
    PyTuple,
};
use pyo3::IntoPyObjectExt;
use serde_json::{json, Value};

create_exception!(pybo, BoError, pyo3::exceptions::PyException);

/// The package version.
#[pyfunction]
fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

// ---------------------------------------------------------------------------
// Timecode
// ---------------------------------------------------------------------------

/// A moment in time, in the one dialect every surface speaks.
///
/// `Timecode(1.23)` and `Timecode("1.23")` are 1.23 seconds (a bare number
/// is seconds); `Timecode("1:02.5")` is a minute and change; `str(t)` is the
/// canonical `HH:MM:SS.fff`. Whole milliseconds ride in `t.ms`.
#[pyclass(module = "pybo", frozen, eq)]
#[derive(PartialEq, Eq)]
struct Timecode {
    ms: u64,
}

#[pymethods]
impl Timecode {
    #[new]
    fn new(value: &Bound<'_, PyAny>) -> PyResult<Self> {
        Ok(Self {
            ms: coerce_ms(value)?,
        })
    }

    /// A Timecode from whole milliseconds (the tree's unit).
    #[staticmethod]
    fn from_ms(ms: u64) -> Self {
        Self { ms }
    }

    /// Whole milliseconds, the wire's unit.
    #[getter]
    fn ms(&self) -> u64 {
        self.ms
    }

    /// Whole seconds.
    #[getter]
    fn seconds(&self) -> f64 {
        self.ms as f64 / 1000.0
    }

    fn __str__(&self) -> String {
        tc_text(self.ms)
    }

    fn __repr__(&self) -> String {
        format!("Timecode('{}')", tc_text(self.ms))
    }
}

// ---------------------------------------------------------------------------
// Material: what to place
// ---------------------------------------------------------------------------

/// A slice of a source, ready to place: `uri` and the `from..to` window of
/// it to play (`to: None` plays to the source's end, resolved by probing
/// when the clip is placed).
#[pyclass(module = "pybo", frozen, eq)]
#[derive(PartialEq, Eq)]
struct Material {
    uri: String,
    from_ms: u64,
    to_ms: Option<u64>,
}

#[pymethods]
impl Material {
    /// The source address.
    #[getter]
    fn uri(&self) -> &str {
        &self.uri
    }

    /// In-point, whole milliseconds into the source.
    #[getter]
    fn from_ms(&self) -> u64 {
        self.from_ms
    }

    /// Out-point, whole milliseconds into the source; `None` = its end.
    #[getter]
    fn to_ms(&self) -> Option<u64> {
        self.to_ms
    }

    fn __repr__(&self) -> String {
        let window = match self.to_ms {
            Some(to) => format!("{}-{}", tc_text(self.from_ms), tc_text(to)),
            None => format!("{}-", tc_text(self.from_ms)),
        };
        format!("trim({:?}, {window:?})", self.uri)
    }
}

impl Material {
    fn to_clip(&self) -> bc::Clip {
        let from = bc::Timecode(Duration::from_millis(self.from_ms));
        match self.to_ms {
            Some(to) => {
                let to = bc::Timecode(Duration::from_millis(to));
                bc::Clip::of(self.uri.clone()).trim(bc::TimecodeRange::closed(from, to))
            }
            None => bc::Clip::of(self.uri.clone()).trim(bc::TimecodeRange::open(from)),
        }
    }
}

/// Slice `uri` to a window of its own time: the whole source, a
/// `from-to` span, an open `from-` tail, or `start`/`to` moments.
///
/// ```python
/// clip = pybo.trim("voice.wav")              # the whole source
/// clip = pybo.trim("voice.wav", "0:30-1:00") # a closed span
/// clip = pybo.trim("voice.wav", "0:30-")     # to the source's end
/// clip = pybo.trim("voice.wav", start="0:30", to="1:00")
/// ```
#[pyfunction]
#[pyo3(signature = (uri, range=None, *, start=None, to=None))]
fn trim(
    uri: String,
    range: Option<&Bound<'_, PyAny>>,
    start: Option<&Bound<'_, PyAny>>,
    to: Option<&Bound<'_, PyAny>>,
) -> PyResult<Material> {
    let (from, end) = match (range, start, to) {
        (Some(text), None, None) => {
            let from_to = parse_range_text(&text.extract::<String>()?)?;
            (ms_of(from_to.0), from_to.1.map(ms_of))
        }
        (None, start, to) => {
            let from = match start {
                Some(s) => coerce_ms(s)?,
                None => 0,
            };
            let to = match to {
                Some(t) => Some(coerce_ms(t)?),
                None => None,
            };
            if let Some(to) = to
                && to < from
            {
                return Err(BoError::new_err(format!(
                    "trim to {} before from {}",
                    tc_text(to),
                    tc_text(from)
                )));
            }
            (from, to)
        }
        _ => {
            return Err(PyValueError::new_err(
                "trim takes a range text or start/to, not both",
            ))
        }
    };
    Ok(Material {
        uri,
        from_ms: from,
        to_ms: end,
    })
}

// ---------------------------------------------------------------------------
// Placement
// ---------------------------------------------------------------------------

/// Where a clip lands: an existing track (created on demand up to its
/// index), or a fresh one. `Track(n)` is track `n` at the playhead;
/// `Track(n).at(t)` at a moment; `Track.fresh(at=t)` a new track at `t`.
#[pyclass(module = "pybo", frozen, eq)]
#[derive(PartialEq, Eq)]
struct Track {
    /// The track's index; `None` = a fresh track.
    index: Option<usize>,
    /// The moment on the track; `None` = the playhead.
    at_ms: Option<u64>,
}

#[pymethods]
impl Track {
    #[new]
    fn new(index: usize) -> Self {
        Self {
            index: Some(index),
            at_ms: None,
        }
    }

    /// A fresh track whose clip lands at `at` (`None` = the playhead).
    #[staticmethod]
    #[pyo3(signature = (at=None))]
    fn fresh(at: Option<&Bound<'_, PyAny>>) -> PyResult<Self> {
        Ok(Self {
            index: None,
            at_ms: match at {
                Some(t) => Some(coerce_ms(t)?),
                None => None,
            },
        })
    }

    /// This track, at a moment.
    fn at(&self, moment: &Bound<'_, PyAny>) -> PyResult<Self> {
        Ok(Self {
            index: self.index,
            at_ms: Some(coerce_ms(moment)?),
        })
    }

    /// The track's index; `None` = a fresh track.
    #[getter]
    fn index(&self) -> Option<usize> {
        self.index
    }

    fn __repr__(&self) -> String {
        let who = match self.index {
            Some(i) => format!("Track({i})"),
            None => "Track.fresh()".to_string(),
        };
        match self.at_ms {
            Some(ms) => format!("{who}.at('{}')", tc_text(ms)),
            None => who,
        }
    }
}

// ---------------------------------------------------------------------------
// Bo
// ---------------------------------------------------------------------------

/// A session client: the arrangement lives in the daemon this client
/// reaches over its Unix socket (`socket`), auto-spawned on demand.
/// `Bo()` uses the shared default socket; tests and isolated sessions pass
/// their own.
///
/// Every verb edits one shared arrangement. A `put` of a whole source is
/// probed (its end resolved) where the arrangement lives; a closed
/// `trim(...)` span touches no disk until it plays or renders.
#[pyclass(module = "pybo")]
struct Bo {
    inner: bc::Bo,
    socket: String,
}

#[pymethods]
impl Bo {
    #[new]
    #[pyo3(signature = (socket=None))]
    fn new(socket: Option<String>) -> PyResult<Self> {
        let (inner, socket) = match socket {
            Some(path) => (
                bc::Bo::with_connection(bo::connection::Connection::at(&path)),
                path,
            ),
            None => {
                let path = bo::connection::Connection::default_socket();
                (
                    bc::Bo::with_connection(bo::connection::Connection::at(&path)),
                    path.display().to_string(),
                )
            }
        };
        Ok(Self { inner, socket })
    }

    /// The socket this client speaks over.
    #[getter]
    fn socket(&self) -> &str {
        &self.socket
    }

    /// Place a clip: `material` is a uri (`"voice.wav"`, the whole source)
    /// or a `trim(...)` slice; `dest` is where it lands — `Track(n)`,
    /// `Track(n).at(t)`, `Track.fresh(at=t)`, or omitted for a fresh track
    /// at the playhead. Returns `{"track", "clip", "landed"}`.
    #[pyo3(signature = (material, dest=None))]
    fn put<'py>(
        &mut self,
        py: Python<'py>,
        material: &Bound<'py, PyAny>,
        dest: Option<&Bound<'py, PyAny>>,
    ) -> PyResult<Py<PyAny>> {
        let clip = material_to_clip(material)?;
        let to = self.destination(dest)?;
        let inserted = self.inner.put(clip, to).map_err(err)?;
        let reply = json!({
            "track": inserted.track,
            "clip": placed_clip(&inserted.clip),
            "landed": landed(inserted.landed),
        });
        json_to_py(py, &reply)
    }

    /// Remove a clip: `on` is the track, addressed by its stable `clip_id`
    /// or by `at` (the clip covering that moment). Returns `{"track",
    /// "clip", "landed"}`.
    #[pyo3(signature = (on, clip_id=None, at=None))]
    fn take<'py>(
        &mut self,
        py: Python<'py>,
        on: usize,
        clip_id: Option<u64>,
        at: Option<&Bound<'py, PyAny>>,
    ) -> PyResult<Py<PyAny>> {
        let target = clip_here(on, clip_id, at)?;
        let removed = self.inner.take(target).map_err(err)?;
        let reply = json!({
            "track": removed.track,
            "clip": placed_clip(&removed.clip),
            "landed": landed(removed.landed),
        });
        json_to_py(py, &reply)
    }

    /// Move a clip (`on`, by stable `clip_id` or the `at` it covers) to a
    /// destination track/position (`to`). Returns `{"from_track",
    /// "to_track", "clip", "landed"}`.
    #[pyo3(signature = (on, clip_id=None, at=None, to=None), name = "move")]
    fn r#move<'py>(
        &mut self,
        py: Python<'py>,
        on: usize,
        clip_id: Option<u64>,
        at: Option<&Bound<'py, PyAny>>,
        to: Option<&Bound<'py, PyAny>>,
    ) -> PyResult<Py<PyAny>> {
        let source = clip_here(on, clip_id, at)?;
        let destination = match to {
            Some(dest) => self.destination(Some(dest))?,
            None => return Err(PyValueError::new_err("move needs a destination `to`")),
        };
        let moved = self.inner.r#move(source, destination).map_err(err)?;
        let reply = json!({
            "from_track": moved.from_track,
            "to_track": moved.to_track,
            "clip": placed_clip(&moved.clip),
            "landed": landed(moved.landed),
        });
        json_to_py(py, &reply)
    }

    /// Route a track's output into a bus: `"master"`, a group bus `id`, or
    /// a `name` (the bus is created by its first mention). Returns
    /// `{"track", "bus", "landed"}`.
    fn route<'py>(
        &mut self,
        py: Python<'py>,
        on: usize,
        bus: &Bound<'py, PyAny>,
    ) -> PyResult<Py<PyAny>> {
        let target = self.bus_index(bus)?;
        let routed = self.inner.route(bc::TrackIndex(on), target).map_err(err)?;
        let bus = match routed.bus {
            bc::BusRef::Master => json!("master"),
            bc::BusRef::Group(id) => json!({"group": id}),
        };
        let reply = json!({
            "track": routed.track,
            "bus": bus,
            "landed": landed(routed.landed),
        });
        json_to_py(py, &reply)
    }

    /// Patch the state zone: a leaf (`"track.0.volume"`), a strip object
    /// (`"track.0"` with `{"volume": 0.4, "muted": true}`), or deep into a
    /// clip's controls. Returns `{"path", "patched", "landed"}`.
    fn set<'py>(
        &mut self,
        py: Python<'py>,
        path: &str,
        value: &Bound<'py, PyAny>,
    ) -> PyResult<Py<PyAny>> {
        let patcher = py_to_json(value)?;
        let set = self.inner.set(path, patcher).map_err(err)?;
        let reply = json!({
            "path": set.path,
            "patched": set.patched,
            "landed": landed(set.landed),
        });
        json_to_py(py, &reply)
    }

    /// Read the arrangement — the whole tree (`""`), a subtree, or a leaf —
    /// as a dict. Times are whole milliseconds; ids are stable per track.
    #[pyo3(signature = (path=""))]
    fn get<'py>(&mut self, py: Python<'py>, path: &str) -> PyResult<Py<PyAny>> {
        let tree = self.inner.get(path).map_err(err)?;
        json_to_py(py, &tree)
    }

    /// Mix the arrangement to a wav file — optionally only a `trim` range
    /// (`"1:00-2:00"`, `"1:00-"`), measured (`measure=True`) or folded to
    /// mono (`mono=True`). Returns `{"file", "duration_ms", "stats"}`.
    #[pyo3(signature = (file, trim=None, measure=false, mono=false))]
    fn render<'py>(
        &mut self,
        py: Python<'py>,
        file: &str,
        trim: Option<&str>,
        measure: bool,
        mono: bool,
    ) -> PyResult<Py<PyAny>> {
        let (from, to) = match trim {
            Some(text) => parse_range_text(text)?,
            None => (Duration::ZERO, None),
        };
        let settings = bc::RenderConfig {
            trim: Some(bc::TimecodeRange {
                from: bc::Timecode(from),
                to: to.map(bc::Timecode),
            }),
            measure,
            mono,
        };
        let rendered = self.inner.render(file, settings).map_err(err)?;
        let stats = match &rendered.stats {
            Some(stats) => json!({
                "span_ms": stats.span_ms,
                "peak_db": stats.peak_db,
                "true_peak_db": stats.true_peak_db,
                "rms_db": stats.rms_db,
                "integrated_lufs": stats.integrated_lufs,
                "momentary_max_lufs": stats.momentary_max_lufs,
                "short_term_max_lufs": stats.short_term_max_lufs,
                "lra": stats.lra,
            }),
            None => Value::Null,
        };
        let reply = json!({
            "file": rendered.file,
            "duration_ms": rendered.duration_ms,
            "stats": stats,
        });
        json_to_py(py, &reply)
    }

    /// Save the session as a snapshot at `path`. Returns
    /// `{"version", "playhead", "commands"}`.
    fn save<'py>(&mut self, py: Python<'py>, path: &str) -> PyResult<Py<PyAny>> {
        let snapshot = self.inner.save(path).map_err(err)?;
        let reply = json!({
            "version": snapshot.version,
            "playhead": ms_of(snapshot.playhead),
            "commands": snapshot.history.len(),
        });
        json_to_py(py, &reply)
    }

    /// Replace the session from a snapshot at `path`, atomically.
    fn load(&mut self, path: &str) -> PyResult<()> {
        self.inner.load(path).map_err(err)
    }

    /// Validate a snapshot file without touching the session.
    fn check(&mut self, path: &str) -> PyResult<()> {
        self.inner.check(path).map_err(err)
    }

    /// Drop every track and group bus: back to a fresh session.
    fn reset(&mut self) -> PyResult<()> {
        self.inner.reset().map_err(err)
    }

    /// Start playback from the current playhead. Returns `{"tracks",
    /// "clips", "end", "playhead"}`.
    fn play<'py>(&mut self, py: Python<'py>) -> PyResult<Py<PyAny>> {
        let played = self.inner.play().map_err(err)?;
        let reply = json!({
            "tracks": played.tracks,
            "clips": played.clips,
            "end": ms_of(played.end),
            "playhead": ms_of(played.playhead),
        });
        json_to_py(py, &reply)
    }

    /// Hold position and silence output; returns `{"at"}`.
    fn pause<'py>(&mut self, py: Python<'py>) -> PyResult<Py<PyAny>> {
        let at = self.inner.pause().map_err(err)?;
        json_to_py(py, &json!({ "at": ms_of(at) }))
    }

    /// Continue after a pause; returns `{"at"}`.
    fn resume<'py>(&mut self, py: Python<'py>) -> PyResult<Py<PyAny>> {
        let at = self.inner.resume().map_err(err)?;
        json_to_py(py, &json!({ "at": ms_of(at) }))
    }

    /// Jump the playhead.
    fn seek(&mut self, at: &Bound<'_, PyAny>) -> PyResult<()> {
        self.inner
            .seek(Duration::from_millis(coerce_ms(at)?))
            .map_err(err)
    }

    /// Stop and rewind to zero.
    fn stop(&mut self) -> PyResult<()> {
        self.inner.stop().map_err(err)
    }

    /// Make every pending edit audible; returns what an `apply` did.
    fn apply<'py>(&mut self, py: Python<'py>) -> PyResult<Py<PyAny>> {
        let applied = self.inner.apply().map_err(err)?;
        use bc::Applied;
        let reply = match applied {
            Applied::Nothing => json!("nothing pending"),
            Applied::Live(n) => json!({ "kind": "live", "count": n }),
            Applied::Rebuilt { live, at } => json!({
                "kind": "rebuilt", "live": live, "at": ms_of(at),
            }),
            Applied::NotPlaying => json!("not playing"),
        };
        json_to_py(py, &reply)
    }
}

impl Bo {
    /// The current playhead, whole milliseconds — where a bare `Track(n)`
    /// places its clip.
    fn playhead_ms(&mut self) -> PyResult<u64> {
        let tree = self.inner.get("transport.playhead").map_err(err)?;
        tree.as_u64()
            .ok_or_else(|| BoError::new_err("transport has no playhead"))
    }

    /// A clip's destination from a Python `dest`: `None` = a fresh track at
    /// the playhead; `Track(n)` = track `n` at the playhead; `Track(n).at(t)`
    /// = there; `Track.fresh(at=t)` = a fresh track at `t`.
    fn destination(&mut self, dest: Option<&Bound<'_, PyAny>>) -> PyResult<bc::Destination> {
        let Some(obj) = dest else {
            return Ok(bc::Destination::NewTrack(bc::NewTrack::default()));
        };
        let track = obj
            .cast::<Track>()
            .map_err(|_| PyTypeError::new_err("expected a Track — Track(n), Track(n).at(t), Track.fresh()"))?;
        let place = track.borrow();
        match (place.index, place.at_ms) {
            (Some(index), Some(ms)) => Ok(bc::Destination::Track(bc::TrackPosition {
                track: index,
                at: Duration::from_millis(ms),
            })),
            (Some(index), None) => {
                let at = Duration::from_millis(self.playhead_ms()?);
                Ok(bc::Destination::Track(bc::TrackPosition {
                    track: index,
                    at,
                }))
            }
            (None, Some(ms)) => Ok(bc::Destination::NewTrack(bc::NewTrack::at(
                bc::Timecode(Duration::from_millis(ms)),
            ))),
            (None, None) => Ok(bc::Destination::NewTrack(bc::NewTrack::default())),
        }
    }

    /// A route target from a Python `bus`: `"master"`, an int group id, or a
    /// name — joined if a bus of that name exists (its id is its position in
    /// the tree), created by its first mention otherwise.
    fn bus_index(&mut self, bus: &Bound<'_, PyAny>) -> PyResult<bc::BusIndex> {
        if let Ok(id) = bus.extract::<u64>() {
            return Ok(bc::BusIndex::group(id));
        }
        let name: String = bus.extract().map_err(|_| {
            PyTypeError::new_err("bus must be 'master', a group id, or a bus name")
        })?;
        if name == "master" {
            return Ok(bc::BusIndex::master());
        }
        // A first mention creates the bus; an existing one is joined.
        let table = self.inner.get("bus").map_err(err)?;
        let existing = table.as_array().map(|buses| {
            buses.iter().position(|bus| {
                bus.get("name").and_then(Value::as_str) == Some(name.as_str())
            })
        });
        match existing {
            Some(Some(index)) => Ok(bc::BusIndex::group(index as u64)),
            _ => Ok(bc::BusIndex::New(bc::NewBus::with_name(name))),
        }
    }
}

// ---------------------------------------------------------------------------
// Conversions
// ---------------------------------------------------------------------------

fn err(e: bc::Error) -> PyErr {
    BoError::new_err(e.to_string())
}

/// Whole milliseconds of a duration, the wire's unit.
fn ms_of(d: Duration) -> u64 {
    d.as_secs() * 1000 + u64::from(d.subsec_millis())
}

/// A duration's canonical text.
fn tc_text(ms: u64) -> String {
    bo::time::format(Duration::from_millis(ms))
}

/// Coerce any timecode-like value to whole milliseconds: a `Timecode`, a
/// number of seconds (`1.23`, an int), or lenient timecode text (`"1:02.5"`,
/// `"1.23"` — a bare number is seconds).
fn coerce_ms(value: &Bound<'_, PyAny>) -> PyResult<u64> {
    if let Ok(tc) = value.cast::<Timecode>() {
        return Ok(tc.borrow().ms);
    }
    if let Ok(text) = value.extract::<String>() {
        let duration = bo::time::parse(&text)
            .map_err(|e| PyValueError::new_err(format!("bad timecode {text:?}: {e}")))?;
        return Ok(ms_of(duration));
    }
    let seconds: f64 = value.extract().map_err(|_| {
        PyTypeError::new_err(
            "expected a Timecode, a number of seconds, or timecode text like '1:02.5'",
        )
    })?;
    if seconds < 0.0 || !seconds.is_finite() {
        return Err(PyValueError::new_err(format!("bad timecode {seconds}")));
    }
    Ok((seconds * 1000.0).round() as u64)
}

/// Parse `from-to` / `from-` range text into a (from, to) pair.
fn parse_range_text(text: &str) -> PyResult<(Duration, Option<Duration>)> {
    let (from, to) = text
        .split_once('-')
        .ok_or_else(|| PyValueError::new_err(format!("bad range {text:?}: expected from-to")))?;
    let from_text = from.trim();
    let to_text = to.trim();
    if from_text.is_empty() {
        return Err(PyValueError::new_err(format!(
            "bad range {text:?}: missing from"
        )));
    }
    let from = bo::time::parse(from_text)
        .map_err(|e| PyValueError::new_err(format!("bad range {text:?}: {e}")))?;
    let to = if to_text.is_empty() {
        None
    } else {
        let parsed = bo::time::parse(to_text)
            .map_err(|e| PyValueError::new_err(format!("bad range {text:?}: {e}")))?;
        if parsed < from {
            return Err(PyValueError::new_err(format!(
                "bad range {text:?}: to before from"
            )));
        }
        Some(parsed)
    };
    Ok((from, to))
}

/// A material from a Python `material`: a bare uri (the whole source) or a
/// `trim(...)` slice.
fn material_to_clip(material: &Bound<'_, PyAny>) -> PyResult<bc::Clip> {
    if let Ok(uri) = material.extract::<String>() {
        return Ok(bc::Clip::of(uri));
    }
    let slice = material
        .cast::<Material>()
        .map_err(|_| PyTypeError::new_err("expected a source uri or a trim(...) slice"))?;
    Ok(slice.borrow().to_clip())
}

/// Address a clip on a track: by stable id, or by the moment it covers.
fn clip_here(
    on: usize,
    clip_id: Option<u64>,
    at: Option<&Bound<'_, PyAny>>,
) -> PyResult<bc::ClipOnTrack> {
    match (clip_id, at) {
        (Some(id), None) => Ok(bc::ClipOnTrack::id(bc::TrackIndex(on), id)),
        (None, Some(at)) => {
            let at = Duration::from_millis(coerce_ms(at)?);
            Ok(bc::ClipOnTrack::at(bc::TrackIndex(on), at))
        }
        (None, None) => Err(PyValueError::new_err("address a clip by clip_id or at")),
        (Some(_), Some(_)) => Err(PyValueError::new_err("give clip_id or at, not both")),
    }
}

/// A placed clip as the reply dict's `clip` — the same shape the tree uses.
fn placed_clip(clip: &bc::PlacedClip) -> Value {
    json!({
        "id": clip.id,
        "uri": clip.uri,
        "at": ms_of(clip.at),
        "from": ms_of(clip.from),
        "to": ms_of(clip.to),
        "gain": clip.gain,
    })
}

/// An edit's landing, as the reply's word.
fn landed(landed: bc::Landed) -> &'static str {
    match landed {
        bc::Landed::Live => "live",
        bc::Landed::Pending => "pending",
    }
}

/// A Python value as JSON: bool, int, float, str, None, list, dict — the
/// shapes `set`'s patcher accepts.
fn py_to_json(obj: &Bound<'_, PyAny>) -> PyResult<Value> {
    if obj.is_none() {
        return Ok(Value::Null);
    }
    if obj.is_instance_of::<PyBool>() {
        return Ok(Value::Bool(obj.extract()?));
    }
    if obj.is_instance_of::<PyInt>() {
        return Ok(Value::from(obj.extract::<i64>()?));
    }
    if obj.is_instance_of::<PyFloat>() {
        return Ok(Value::from(obj.extract::<f64>()?));
    }
    if let Ok(text) = obj.extract::<String>() {
        return Ok(Value::String(text));
    }
    if obj.is_instance_of::<PyList>() || obj.is_instance_of::<PyTuple>() {
        let mut items = Vec::new();
        for item in obj.try_iter()? {
            items.push(py_to_json(&item?)?);
        }
        return Ok(Value::Array(items));
    }
    if obj.is_instance_of::<PyDict>() {
        let dict = obj.cast::<PyDict>()?;
        let mut fields = serde_json::Map::new();
        for (key, value) in dict.iter() {
            let key: String = key.extract().map_err(|_| {
                PyTypeError::new_err("dict keys must be strings for a patch")
            })?;
            fields.insert(key, py_to_json(&value)?);
        }
        return Ok(Value::Object(fields));
    }
    Err(PyTypeError::new_err(
        "expected a scalar (bool/int/float/str/None), a list, or a dict",
    ))
}

/// A JSON value as a Python object.
fn json_to_py<'py>(py: Python<'py>, value: &Value) -> PyResult<Py<PyAny>> {
    Ok(match value {
        Value::Null => py.None(),
        Value::Bool(b) => (*b).into_py_any(py)?,
        Value::Number(n) => {
            if let Some(u) = n.as_u64() {
                u.into_py_any(py)?
            } else if let Some(i) = n.as_i64() {
                i.into_py_any(py)?
            } else {
                n.as_f64().expect("a finite json number").into_py_any(py)?
            }
        }
        Value::String(s) => s.as_str().into_py_any(py)?,
        Value::Array(items) => {
            let list = PyList::empty(py);
            for item in items {
                list.append(json_to_py(py, item)?)?;
            }
            list.into_any().unbind()
        }
        Value::Object(fields) => {
            let dict = PyDict::new(py);
            for (key, item) in fields {
                dict.set_item(key, json_to_py(py, item)?)?;
            }
            dict.into_any().unbind()
        }
    })
}

// ---------------------------------------------------------------------------
// The module
// ---------------------------------------------------------------------------

#[pymodule]
fn pybo(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_function(wrap_pyfunction!(version, m)?)?;
    m.add_function(wrap_pyfunction!(trim, m)?)?;
    m.add_class::<Timecode>()?;
    m.add_class::<Material>()?;
    m.add_class::<Track>()?;
    m.add_class::<Bo>()?;
    m.add("BoError", m.py().get_type::<BoError>())?;
    Ok(())
}
