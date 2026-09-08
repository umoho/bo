//! The control sources — the cables that modulate a parameter as its clip
//! plays, spoken as one-line JSON objects.
//!
//! A parameter is the static base plus the sum of its active sources: the
//! hand-drawn curve ([`Curve`]) is a map of time-to-offset points, the
//! low-frequency oscillator ([`Lfo`]) is a periodic wiggle, and the sidechain
//! ([`Sidechain`]) follows another bus's level. The registry is
//! [`ControlSource`]. Everything here is data and its text form — no DSP;
//! the engine builds these into the mix. The layer depends on [`crate::bus`]
//! (a sidechain listens to a bus), [`crate::time`] (durations are timecodes)
//! and nothing else.
//!
//! # The JSON form
//!
//! Every source is one JSON object on one line; the `"type"` field says which
//! kind it is and nothing is inferred:
//!
//! * a curve is a map of timecode-to-offset points —
//!   `{"type":"curve","00:00:00.000":1.0,"00:00:03.200":-1.0}` — linear
//!   between the points and held flat beyond them;
//! * an LFO is `{"type":"lfo","shape":"sine","rate":1,"depth":0.5,"phase":0}`
//!   — every field may be omitted and falls back to a default;
//! * a sidechain is
//!   `{"type":"sidechain","bus":"master","amount":-0.5,"attack":"0.005","release":"0.12"}`
//!   — a duck under a bus, by its stable id (`"master"` or `"group.N"`).
//!
//! A timecode may be typed in any of the forms [`crate::time::parse`]
//! accepts — `3.2` and `0.005` mean seconds — and echoes back as
//! `HH:MM:SS.fff`. Numbers echo in their shortest round-tripping text
//! (`1`, not `1.0`).
use std::time::Duration;

use serde_json::Value;

use crate::time;

/// One breakpoint of a [`Curve`]: the offset it outputs at a clip-local
/// timecode. Clip-local, so a curve rides its clip: move the clip and the
/// whole curve moves with it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Keyframe {
    /// Where on the clip this point sits, measured from the clip's start.
    pub at: Duration,
    /// The offset the source outputs here — an add-on to the parameter's
    /// static base, not an absolute value.
    pub value: f32,
}

/// A hand-drawn curve: the first control source (in GStreamer terms, a src
/// — it only produces). Its signal is a scalar offset over the clip's own
/// time, linear between keyframes and held flat beyond the first and last,
/// so a clip that carries one has a value at every moment it plays. The
/// broader notion — using curves to drive parameters as they play, live and
/// rendered alike — is *automation*; this struct is the concrete curve a
/// curve-automation is built on.
///
/// A curve knows nothing about which parameter it drives or what that
/// parameter allows (the jack clamps); it only answers "what offset at this
/// moment". An empty curve is silence: an offset of zero.
#[derive(Debug, Clone, PartialEq)]
pub struct Curve {
    keyframes: Vec<Keyframe>,
}

impl Curve {
    /// A curve over the given breakpoints. They are sorted by time on the
    /// way in, so authoring order never matters; at a time shared by two
    /// keyframes the later one wins.
    pub fn new(mut keyframes: Vec<Keyframe>) -> Self {
        keyframes.sort_by_key(|k| k.at);
        Self { keyframes }
    }

    /// Whether the curve carries no breakpoints — and so no signal.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.keyframes.is_empty()
    }

    /// The breakpoints, in time order.
    #[must_use]
    pub fn keyframes(&self) -> &[Keyframe] {
        &self.keyframes
    }

    /// The curve's signal at clip-local time `t`: linear between two
    /// keyframes, held flat before the first and at the last. Empty — no
    /// signal — is zero.
    #[must_use]
    pub fn value_at(&self, t: Duration) -> f32 {
        let ks = &self.keyframes;
        let Some(first) = ks.first() else {
            return 0.0;
        };
        if ks.len() == 1 {
            return first.value;
        }
        if t < first.at {
            return first.value;
        }
        for pair in ks.windows(2) {
            let a = &pair[0];
            let b = &pair[1];
            if t >= a.at && t < b.at {
                if b.at == a.at {
                    return b.value; // unreachable in sorted input, kept honest
                }
                let x = (t - a.at).as_secs_f64() / (b.at - a.at).as_secs_f64();
                return a.value + (b.value - a.value) * x as f32;
            }
        }
        ks.last().expect("nonempty").value
    }
}

impl std::fmt::Display for Curve {
    /// `{"type":"curve","00:00:00.000":1,"00:00:03.200":-1}` — each point is
    /// a clip-local timecode and the offset it outputs, in time order.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{{\"type\":\"curve\"")?;
        for k in &self.keyframes {
            write!(f, ",\"{}\":{}", time::format(k.at), num(k.value))?;
        }
        f.write_str("}")
    }
}

impl std::str::FromStr for Curve {
    type Err = String;

    /// `{"type":"curve",Tc:V,...}` — every other key after the type is a
    /// timecode mapping to an offset.
    fn from_str(s: &str) -> Result<Self, String> {
        curve_from_value(&json_value(s)?)
    }
}

/// The shape of an [`Lfo`]'s cycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LfoShape {
    /// A smooth sine swing.
    #[default]
    Sine,
    /// A linear triangle.
    Triangle,
    /// A hard square.
    Square,
}

impl LfoShape {
    /// The shape sampled at cycle phase `p` in `0.0 .. 1.0`, in `-1 ..= 1`.
    #[must_use]
    pub fn sample(self, p: f32) -> f32 {
        let p = p - p.floor(); // keep it in the unit cycle
        match self {
            Self::Sine => (p * std::f32::consts::TAU).sin(),
            Self::Triangle => {
                if p < 0.25 {
                    4.0 * p
                } else if p < 0.75 {
                    2.0 - 4.0 * p
                } else {
                    4.0 * p - 4.0
                }
            }
            Self::Square => {
                if p < 0.5 {
                    1.0
                } else {
                    -1.0
                }
            }
        }
    }
}

impl std::fmt::Display for LfoShape {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Sine => "sine",
            Self::Triangle => "triangle",
            Self::Square => "square",
        })
    }
}

impl std::str::FromStr for LfoShape {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, String> {
        match s.trim() {
            "sine" => Ok(Self::Sine),
            "triangle" => Ok(Self::Triangle),
            "square" => Ok(Self::Square),
            other => Err(format!("bad shape {other:?}: sine, triangle or square")),
        }
    }
}

/// A low-frequency oscillator — a periodic control source (a GStreamer-style
/// src like the curve, producing where the samples flow). Its offset at any
/// clip-local moment is `depth` times its shape at the cycle reached by
/// `rate` since the clip started, nudged by `phase`: a bipolar wiggle the
/// parameter's static base rides, clamped at the parameter's own field.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Lfo {
    /// Cycles per second, `> 0`.
    pub rate: f32,
    /// Peak offset, `0.0 ..= 1.0`; half by default.
    pub depth: f32,
    /// The shape of each cycle.
    pub shape: LfoShape,
    /// Where in the cycle the clip starts, in cycles, `0.0 .. 1.0`.
    pub phase: f32,
}

impl Default for Lfo {
    fn default() -> Self {
        Self {
            rate: 1.0,
            depth: 0.5,
            shape: LfoShape::Sine,
            phase: 0.0,
        }
    }
}

impl Lfo {
    /// An LFO over the given rate, depth, shape and start phase, normalized
    /// on the way in (rate kept positive, depth clamped to `0..=1`, phase
    /// wrapped to one cycle).
    #[must_use]
    pub fn new(rate: f32, depth: f32, shape: LfoShape, phase: f32) -> Self {
        Self {
            rate: rate.abs().max(f32::EPSILON),
            depth: depth.clamp(0.0, 1.0),
            shape,
            phase: phase - phase.floor(),
        }
    }

    /// The LFO's signal at clip-local time `t` — an offset on top of the
    /// parameter's static base, in `-depth ..= depth`.
    #[must_use]
    pub fn value_at(&self, t: Duration) -> f32 {
        let cycles = t.as_secs_f64() as f32 * self.rate;
        self.depth * self.shape.sample(self.phase + cycles)
    }
}

impl std::fmt::Display for Lfo {
    /// `{"type":"lfo","shape":"sine","rate":1,"depth":0.5,"phase":0}` —
    /// every field echoes back, defaults included.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{{\"type\":\"lfo\",\"shape\":\"{}\",\"rate\":{},\"depth\":{},\"phase\":{}}}",
            self.shape,
            num(self.rate),
            num(self.depth),
            num(self.phase)
        )
    }
}

impl std::str::FromStr for Lfo {
    type Err = String;

    /// `{"type":"lfo",...}` — shape, rate (cycles per second), depth and
    /// phase are all optional; each falls back to its default.
    fn from_str(s: &str) -> Result<Self, String> {
        lfo_from_value(&json_value(s)?)
    }
}

/// A ducking (or swelling) source: it listens to another bus — the master
/// or a group bus — follows its level with an envelope detector, and emits
/// `amount` times that level as an offset. Negative amount ducks the
/// parameter as the listened bus gets loud (music under a voice, the
/// radio gesture); positive swells with it. The audio comes from the
/// graph's wiring, not from the source's own time, so this source carries
/// no signal until it is built into a mix that has the bus it listens to.
#[derive(Debug, Clone, PartialEq)]
pub struct Sidechain {
    /// The bus whose level is followed: the master, or a group bus by id.
    pub listen: crate::bus::BusRef,
    /// Offset per unit level, signed: negative ducks, positive swells.
    pub amount: f32,
    /// How fast the level rises when the listened signal jumps.
    pub attack: Duration,
    /// How fast it falls back when the signal goes quiet.
    pub release: Duration,
}

impl Default for Sidechain {
    fn default() -> Self {
        Self {
            listen: crate::bus::BusRef::Master,
            amount: -0.5,
            attack: Duration::from_millis(5),
            release: Duration::from_millis(150),
        }
    }
}

impl Sidechain {
    /// A sidechain over the given bus, offset per unit level, and detector
    /// time constants.
    #[must_use]
    pub fn new(
        listen: crate::bus::BusRef,
        amount: f32,
        attack: Duration,
        release: Duration,
    ) -> Self {
        Self {
            listen,
            amount,
            attack,
            release,
        }
    }
}

impl std::fmt::Display for Sidechain {
    /// `{"type":"sidechain","bus":"group.1","amount":-0.4,"attack":"00:00:00.010","release":"00:00:00.200"}`
    /// — bus is `master` or `group.N`, the time constants timecodes.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let bus = match self.listen {
            crate::bus::BusRef::Master => "master".to_string(),
            crate::bus::BusRef::Group(id) => format!("group.{id}"),
        };
        write!(
            f,
            "{{\"type\":\"sidechain\",\"bus\":\"{bus}\",\"amount\":{},\"attack\":\"{}\",\"release\":\"{}\"}}",
            num(self.amount),
            time::format(self.attack),
            time::format(self.release)
        )
    }
}

impl std::str::FromStr for Sidechain {
    type Err = String;

    /// `{"type":"sidechain",...}` — bus, amount, attack and release are
    /// optional; a missing bus listens to the master, and the time constants
    /// default to a fast attack and a slow release. `attack` and `release`
    /// accept a timecode string or a plain number of seconds.
    fn from_str(s: &str) -> Result<Self, String> {
        sidechain_from_value(&json_value(s)?)
    }
}

/// A control source — the "cable" plugged into a parameter's input. Every
/// source is a scalar over the clip's own time, produced where the samples
/// flow; the parameter it drives is the static base plus the sum of its
/// active sources (a parameter with no cable is just its base).
///
/// v1 ships three kinds of source: the hand-drawn curve, the low-frequency
/// oscillator, and the sidechain (an envelope follower listening to another
/// bus). Automation is the notion of using a source to drive a parameter;
/// this enum is the registry of the concrete sources automation can draw on.
#[derive(Debug, Clone, PartialEq)]
pub enum ControlSource {
    /// A hand-drawn keyframe curve ([`Curve`]).
    Curve(Curve),
    /// A periodic oscillator ([`Lfo`]).
    Lfo(Lfo),
    /// An envelope follower on another bus ([`Sidechain`]).
    Sidechain(Sidechain),
}

impl ControlSource {
    /// This source's own signal at clip-local time `t` — an offset on top
    /// of the parameter's static base. Curve and LFO answer from their own
    /// time; a sidechain's signal comes from the mix it is wired into, so
    /// without that wiring it contributes nothing.
    #[must_use]
    pub fn value_at(&self, t: Duration) -> f32 {
        match self {
            Self::Curve(curve) => curve.value_at(t),
            Self::Lfo(lfo) => lfo.value_at(t),
            Self::Sidechain(..) => 0.0,
        }
    }
}

impl std::fmt::Display for ControlSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Curve(curve) => curve.fmt(f),
            Self::Lfo(lfo) => lfo.fmt(f),
            Self::Sidechain(side) => side.fmt(f),
        }
    }
}

impl std::str::FromStr for ControlSource {
    type Err = String;

    /// One JSON control source: a curve (`{"type":"curve","0":1,"3.2":-1}`),
    /// an LFO (`{"type":"lfo","shape":"sine","rate":1}`) or a sidechain
    /// (`{"type":"sidechain","bus":"group.0"}`). The type is explicit;
    /// nothing is inferred.
    fn from_str(s: &str) -> Result<Self, String> {
        let value = json_value(s)?;
        let obj = value
            .as_object()
            .ok_or_else(|| "a control source is a JSON object".to_string())?;
        match obj.get("type").and_then(Value::as_str) {
            Some("curve") => Ok(Self::Curve(curve_from_value(&value)?)),
            Some("lfo") => Ok(Self::Lfo(lfo_from_value(&value)?)),
            Some("sidechain") => Ok(Self::Sidechain(sidechain_from_value(&value)?)),
            _ => Err(format!(
                "bad control source {s:?}: expected curve, lfo or sidechain \
                 (a JSON object whose \"type\" says which)"
            )),
        }
    }
}

// ---------------------------------------------------------------------------
// JSON plumbing
// ---------------------------------------------------------------------------

/// Parse the whole text as one JSON value.
fn json_value(s: &str) -> Result<Value, String> {
    serde_json::from_str(s).map_err(|_| {
        format!(
            "bad control source {s:?}: expected curve, lfo or sidechain — \
             one JSON object, e.g. {{\"type\":\"curve\",\"0\":1,\"3.2\":-1}}"
        )
    })
}

/// A numeric field: any JSON number, kept finite.
fn field(value: &Value, what: &str) -> Result<f32, String> {
    value
        .as_f64()
        .map(|v| v as f32)
        .filter(|v| v.is_finite())
        .ok_or_else(|| format!("bad {what}: expected a number"))
}

/// A duration field: a timecode string, or a plain number of seconds.
fn duration(value: &Value, what: &str) -> Result<Duration, String> {
    match value {
        Value::String(s) => time::parse(s).map_err(|_| format!("bad {what}: expected a timecode")),
        _ => {
            let secs = value
                .as_f64()
                .filter(|v| v.is_finite() && *v >= 0.0)
                .ok_or_else(|| format!("bad {what}: expected a timecode or seconds"))?;
            Duration::try_from_secs_f64(secs).map_err(|_| format!("bad {what}: too large"))
        }
    }
}

/// A curve from an already-parsed object: every key but `type` is a timecode.
fn curve_from_value(value: &Value) -> Result<Curve, String> {
    let obj = value
        .as_object()
        .ok_or_else(|| "a curve is a JSON object".to_string())?;
    check_type(obj, "curve")?;
    let mut keyframes = Vec::new();
    for (at, v) in obj {
        if at == "type" {
            continue;
        }
        let at = at.as_str();
        let at = time::parse(at)
            .map_err(|_| format!("bad curve time {at:?}: a timecode like \"3.2\" or \"00:00:03.200\""))?;
        let value = field(v, "curve value")
            .map_err(|e| format!("at {}: {e}", time::format(at)))?;
        keyframes.push(Keyframe { at, value });
    }
    Ok(Curve::new(keyframes))
}

/// An LFO from an already-parsed object; every field may be omitted.
fn lfo_from_value(value: &Value) -> Result<Lfo, String> {
    let obj = value
        .as_object()
        .ok_or_else(|| "an lfo is a JSON object".to_string())?;
    check_type(obj, "lfo")?;
    let mut rate = 1.0;
    let mut depth = 0.5;
    let mut shape = LfoShape::Sine;
    let mut phase = 0.0;
    for (key, value) in obj {
        match key.as_str() {
            "type" => {}
            "shape" => {
                shape = value
                    .as_str()
                    .ok_or_else(|| "bad lfo shape: expected a name".to_string())?
                    .parse()?;
            }
            "rate" => rate = field(value, "lfo rate")?,
            "depth" => depth = field(value, "lfo depth")?,
            "phase" => phase = field(value, "lfo phase")?,
            other => {
                return Err(format!(
                    "unknown lfo field {other:?}: shape, rate, depth or phase"
                ))
            }
        }
    }
    Ok(Lfo::new(rate, depth, shape, phase))
}

/// A sidechain from an already-parsed object; every field may be omitted.
fn sidechain_from_value(value: &Value) -> Result<Sidechain, String> {
    let obj = value
        .as_object()
        .ok_or_else(|| "a sidechain is a JSON object".to_string())?;
    check_type(obj, "sidechain")?;
    let mut listen = crate::bus::BusRef::Master;
    let mut amount = -0.5;
    let mut attack = Duration::from_millis(5);
    let mut release = Duration::from_millis(150);
    for (key, value) in obj {
        match key.as_str() {
            "type" => {}
            "bus" => {
                let bus = value
                    .as_str()
                    .ok_or_else(|| "bad sidechain bus: expected a name".to_string())?;
                listen = match bus {
                    "master" => crate::bus::BusRef::Master,
                    other => match other.strip_prefix("group.") {
                        Some(id) => {
                            let id: u64 = id.parse().map_err(|_| {
                                format!("bad sidechain bus {other:?}: master or group.N")
                            })?;
                            crate::bus::BusRef::Group(id)
                        }
                        None => {
                            return Err(format!(
                                "bad sidechain bus {other:?}: master or group.N"
                            ))
                        }
                    },
                };
            }
            "amount" => amount = field(value, "sidechain amount")?,
            "attack" => attack = duration(value, "sidechain attack")?,
            "release" => release = duration(value, "sidechain release")?,
            other => {
                return Err(format!(
                    "unknown sidechain field {other:?}: bus, amount, attack or release"
                ))
            }
        }
    }
    Ok(Sidechain::new(listen, amount, attack, release))
}

/// The object's own `type` field must name the kind being parsed: nothing
/// is inferred, even when a source is parsed as itself.
fn check_type(obj: &serde_json::Map<String, Value>, kind: &str) -> Result<(), String> {
    match obj.get("type").and_then(Value::as_str) {
        Some(actual) if actual == kind => Ok(()),
        _ => Err(format!("bad control source: expected type {kind:?}")),
    }
}

/// A number in its shortest round-tripping text — `1` for `1.0`, not `1.0`.
fn num(v: f32) -> String {
    format!("{v}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bus::BusRef;
    use std::time::Duration;

    fn secs(s: u64) -> Duration {
        Duration::from_secs(s)
    }

    #[test]
    fn a_curve_interpolates_between_keyframes_and_holds_outside() {
        let secs = |s: u64| Duration::from_secs(s);
        let curve = Curve::new(vec![
            Keyframe { at: Duration::ZERO, value: 1.0 },
            Keyframe { at: secs(4), value: -1.0 },
        ]);
        assert_eq!(curve.value_at(Duration::ZERO), 1.0);
        assert!((curve.value_at(secs(2)) - 0.0).abs() < 1e-6, "linear midpoint");
        assert!((curve.value_at(secs(3)) - (-0.5)).abs() < 1e-6);
        assert_eq!(curve.value_at(secs(4)), -1.0);
        // Outside the span the edges hold flat — before the first point too,
        // when the curve does not start at the clip's origin.
        assert_eq!(curve.value_at(secs(10)), -1.0);
        let late = Curve::new(vec![
            Keyframe { at: secs(2), value: 0.5 },
            Keyframe { at: secs(4), value: -0.5 },
        ]);
        assert_eq!(late.value_at(secs(1)), 0.5, "flat before the first point");
    }

    #[test]
    fn a_curve_sorts_authoring_order_and_a_single_point_is_constant() {
        let secs = |s: u64| Duration::from_secs(s);
        // Written back to front: sorted on the way in.
        let curve = Curve::new(vec![
            Keyframe { at: secs(3), value: -1.0 },
            Keyframe { at: secs(1), value: 1.0 },
            Keyframe { at: secs(2), value: 0.0 },
        ]);
        let ats: Vec<u64> = curve.keyframes().iter().map(|k| k.at.as_secs()).collect();
        assert_eq!(ats, vec![1, 2, 3]);
        assert!((curve.value_at(secs(2)) - 0.0).abs() < 1e-6);
        // One point: that value everywhere.
        let flat = Curve::new(vec![Keyframe { at: Duration::ZERO, value: -0.5 }]);
        assert_eq!(flat.value_at(secs(9)), -0.5);
        // Empty: silence, an offset of zero.
        assert_eq!(Curve::new(Vec::new()).value_at(secs(1)), 0.0);
    }

    #[test]
    fn a_curve_round_trips_through_its_json() {
        let curve = Curve::new(vec![
            Keyframe {
                at: Duration::from_secs_f64(0.0),
                value: 1.0,
            },
            Keyframe {
                at: Duration::from_secs_f64(3.2),
                value: -1.0,
            },
            Keyframe {
                at: Duration::from_secs_f64(4.0),
                value: 0.5,
            },
        ]);
        // A curve is a map: every key after the type is a timecode, echoed
        // in time order with the shortest number text.
        let text = curve.to_string();
        assert_eq!(
            text,
            "{\"type\":\"curve\",\"00:00:00.000\":1,\"00:00:03.200\":-1,\"00:00:04.000\":0.5}",
            "{text}"
        );
        assert_eq!(text.parse::<Curve>().unwrap(), curve, "display and parse agree");
        // Seconds shorthand parses too, and object key order never matters.
        assert_eq!(
            "{\"type\":\"curve\",\"00:00:04.000\":0.5,\"3.2\":-1,\"0\":1}"
                .parse::<Curve>()
                .unwrap(),
            curve,
            "timecodes or bare seconds, in any order"
        );
        assert_eq!(
            "{\"type\":\"curve\"}".parse::<Curve>().unwrap(),
            Curve::new(Vec::new())
        );
        assert!("curve".parse::<Curve>().is_err(), "the JSON type must be explicit");
        assert!("{\"type\":\"curve\",\"x\":1}".parse::<Curve>().is_err());
        assert!("{\"type\":\"curve\",\"1\":\"x\"}".parse::<Curve>().is_err());
        assert!("{\"type\":\"curve\",\"-1\":0}".parse::<Curve>().is_err(), "negative time is refused");
        assert!(
            "{\"type\":\"curve\",\"1\":0.5,\"points\":[]}".parse::<Curve>().is_err(),
            "a non-timecode key is refused"
        );
    }

    #[test]
    fn a_control_source_delegates_to_its_curve() {
        let curve = Curve::new(vec![
            Keyframe { at: Duration::ZERO, value: 1.0 },
            Keyframe { at: Duration::from_secs(2), value: -1.0 },
        ]);
        let source = ControlSource::Curve(curve);
        assert_eq!(source.value_at(Duration::from_secs(1)), 0.0);
        assert_eq!(
            source.to_string(),
            "{\"type\":\"curve\",\"00:00:00.000\":1,\"00:00:02.000\":-1}"
        );
        assert_eq!(
            "{\"type\":\"curve\",\"00:00:00.000\":1,\"00:00:02.000\":-1}"
                .parse::<ControlSource>()
                .unwrap(),
            source
        );
    }

    #[test]
    fn an_lfo_shape_samples_its_cycle() {
        use LfoShape::*;
        let close = |a: f32, b: f32| (a - b).abs() < 1e-6;
        assert!(close(Sine.sample(0.0), 0.0));
        assert!(close(Sine.sample(0.25), 1.0));
        assert!(close(Sine.sample(0.5), 0.0));
        assert!(close(Sine.sample(0.75), -1.0));
        // Triangle: linear up 0->1, down to -1, back to 0.
        assert!(close(Triangle.sample(0.0), 0.0));
        assert!(close(Triangle.sample(0.125), 0.5));
        assert!(close(Triangle.sample(0.25), 1.0));
        assert!(close(Triangle.sample(0.5), 0.0));
        assert!(close(Triangle.sample(0.75), -1.0));
        assert!(close(Triangle.sample(0.9), -0.4));
        // Square: the sign of the half-cycle.
        assert_eq!(Square.sample(0.0), 1.0);
        assert_eq!(Square.sample(0.49), 1.0);
        assert_eq!(Square.sample(0.5), -1.0);
        // Phase wraps inside the unit cycle.
        assert!(close(Sine.sample(1.25), 1.0));
    }

    #[test]
    fn an_lfo_oscillates_over_time_and_is_normalized_on_the_way_in() {
        let s = Lfo::new(1.0, 0.5, LfoShape::Sine, 0.0);
        let t = |secs: f64| Duration::from_secs_f64(secs);
        assert!((s.value_at(t(0.0)) - 0.0).abs() < 1e-6);
        assert!((s.value_at(t(0.25)) - 0.5).abs() < 1e-6, "peak at a quarter cycle");
        assert!((s.value_at(t(0.75)) + 0.5).abs() < 1e-6);
        // Phase shifts where in the cycle the clip starts.
        let peaked = Lfo::new(1.0, 0.5, LfoShape::Sine, 0.25);
        assert!((peaked.value_at(t(0.0)) - 0.5).abs() < 1e-6, "starts at its peak");
        // Normalization: rate stays positive, depth clamps, phase wraps.
        let rough = Lfo::new(-1.0, 2.0, LfoShape::Sine, 1.25);
        assert_eq!(rough.rate, 1.0);
        assert_eq!(rough.depth, 1.0);
        assert!((rough.phase - 0.25).abs() < 1e-6);
    }

    #[test]
    fn an_lfo_round_trips_through_its_json() {
        let lfo = Lfo::new(0.5, 0.8, LfoShape::Triangle, 0.25);
        assert_eq!(
            lfo.to_string(),
            "{\"type\":\"lfo\",\"shape\":\"triangle\",\"rate\":0.5,\"depth\":0.8,\"phase\":0.25}"
        );
        assert_eq!(
            "{\"type\":\"lfo\",\"shape\":\"triangle\",\"rate\":0.5,\"depth\":0.8,\"phase\":0.25}"
                .parse::<Lfo>()
                .unwrap(),
            lfo
        );
        // Fields default and may come in any order.
        assert_eq!(
            "{\"type\":\"lfo\",\"depth\":0.4,\"rate\":2}".parse::<Lfo>().unwrap(),
            Lfo::new(2.0, 0.4, LfoShape::Sine, 0.0),
            "shape and phase fall back to their defaults"
        );
        assert_eq!(
            "{\"type\":\"lfo\"}".parse::<Lfo>().unwrap(),
            Lfo::default(),
            "an empty lfo is the default lfo"
        );
        assert!(
            "{\"type\":\"lfo\",\"rate\":0}".parse::<Lfo>().unwrap().rate > 0.0,
            "a zero rate is tamed"
        );
        assert!("{\"type\":\"lfo\",\"rate\":-2}".parse::<Lfo>().unwrap().rate > 0.0);
        assert!("{\"type\":\"lfo\",\"rate\":\"x\"}".parse::<Lfo>().is_err());
        assert!("{\"type\":\"lfo\",\"shape\":\"wibble\"}".parse::<Lfo>().is_err());
        assert!("{\"type\":\"lfo\",\"volume\":1}".parse::<Lfo>().is_err(), "unknown fields are refused");
        assert!("{\"type\":\"wibble\"}".parse::<Lfo>().is_err());
    }

    #[test]
    fn a_control_source_is_typed_explicitly() {
        let lfo = ControlSource::Lfo(Lfo::new(1.0, 0.5, LfoShape::Sine, 0.0));
        assert_eq!(
            lfo.to_string(),
            "{\"type\":\"lfo\",\"shape\":\"sine\",\"rate\":1,\"depth\":0.5,\"phase\":0}"
        );
        assert_eq!(
            "{\"type\":\"lfo\",\"shape\":\"sine\",\"rate\":1,\"depth\":0.5,\"phase\":0}"
                .parse::<ControlSource>()
                .unwrap(),
            lfo
        );
        // The leading type decides; nothing is inferred from the text.
        let curve = ControlSource::Curve(Curve::new(vec![
            Keyframe { at: Duration::ZERO, value: 1.0 },
            Keyframe { at: secs(2), value: -1.0 },
        ]));
        assert_eq!(
            "{\"type\":\"curve\",\"00:00:00.000\":1,\"00:00:02.000\":-1}"
                .parse::<ControlSource>()
                .unwrap(),
            curve
        );
        assert_eq!(curve.to_string(), "{\"type\":\"curve\",\"00:00:00.000\":1,\"00:00:02.000\":-1}");
        assert!("wibble".parse::<ControlSource>().is_err());
        assert!("{\"type\":\"wibble\",\"rate\":1}".parse::<ControlSource>().is_err());
        // The offset at the peak of the first cycle.
        assert!((lfo.value_at(Duration::from_secs_f64(0.25)) - 0.5).abs() < 1e-6);
    }

    #[test]
    fn a_sidechain_round_trips_through_its_json() {
        let duck = Sidechain::new(
            BusRef::Group(1),
            -0.4,
            Duration::from_secs_f64(0.01),
            Duration::from_secs_f64(0.2),
        );
        assert_eq!(
            duck.to_string(),
            "{\"type\":\"sidechain\",\"bus\":\"group.1\",\"amount\":-0.4,\
             \"attack\":\"00:00:00.010\",\"release\":\"00:00:00.200\"}"
        );
        assert_eq!(
            "{\"type\":\"sidechain\",\"bus\":\"group.1\",\"amount\":-0.4,\
             \"attack\":\"0.01\",\"release\":\"0.2\"}"
                .parse::<Sidechain>()
                .unwrap(),
            duck,
            "attack and release take seconds shorthand"
        );
        assert_eq!(
            "{\"type\":\"sidechain\"}".parse::<Sidechain>().unwrap(),
            Sidechain::default(),
            "everything defaults: it listens to the master at -0.5"
        );
        let boost = "{\"type\":\"sidechain\",\"amount\":0.3,\"attack\":0.05}"
            .parse::<Sidechain>()
            .unwrap();
        assert_eq!(boost.amount, 0.3, "an amount may swell as well as duck");
        assert_eq!(boost.attack, Duration::from_millis(50));
        assert_eq!(boost.release, Duration::from_millis(150));
        assert_eq!(boost.listen, BusRef::Master);

        // Wired nowhere yet, a sidechain contributes nothing on its own.
        let source = ControlSource::Sidechain(duck.clone());
        assert_eq!(source.value_at(secs(1)), 0.0);
        assert_eq!(source.to_string(), duck.to_string());
        assert_eq!(source.to_string().parse::<ControlSource>().unwrap(), source);

        for bad in [
            "{\"type\":\"sidechain\",\"bus\":\"grup.1\"}",
            "{\"type\":\"sidechain\",\"bus\":\"group.x\"}",
            "{\"type\":\"sidechain\",\"bus\":1}",
            "{\"type\":\"sidechain\",\"amount\":\"nan\"}",
            "{\"type\":\"sidechain\",\"amount\":-0.4,\"extra\":1}",
        ] {
            assert!(bad.parse::<Sidechain>().is_err(), "{bad} should be refused");
        }
    }

    #[test]
    fn canonical_text_is_one_line_json() {
        // The canonical form is single-line JSON, and it round-trips.
        for src in [
            ControlSource::Curve(Curve::new(vec![
                Keyframe { at: Duration::ZERO, value: 1.0 },
                Keyframe {
                    at: Duration::from_secs_f64(3.2),
                    value: -1.0,
                },
            ])),
            ControlSource::Lfo(Lfo::default()),
            ControlSource::Sidechain(Sidechain::default()),
        ] {
            let text = src.to_string();
            assert!(text.starts_with('{') && text.ends_with('}'), "{text}");
            assert!(!text.contains('\n'), "one line: {text}");
            assert_eq!(text.parse::<ControlSource>().unwrap(), src, "{text}");
        }
    }
}
