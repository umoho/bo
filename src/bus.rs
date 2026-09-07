//! The signal layer: a track's output edge ([`Output`]) and the bus nodes
//! ([`Bus`]) it feeds into.
//!
//! Kept apart from the content layer ([`crate::track`]) on purpose: a
//! [`Track`](crate::track::Track) there is a container of clips on a
//! timeline — *what* plays, *when* — and nothing else; this module is *where
//! the sound goes*. A track reaches into the signal layer through exactly one
//! edge, its output, which carries the strip (gain, mute, placement) and
//! points at a bus. Today that is one bus, `Master`; a `Group` bus between the
//! tracks and the master is the shape a later "route several tracks into one
//! strip" feature grows into, and a layout beyond stereo is where 5.1/7.1
//! would land. None of those exist yet; the enums keep their seams.

/// Where an output lands: which bus receives its signal.
///
/// v1 has exactly one bus, so every output points at the master. A group bus
/// would be a second variant carrying the bus's identity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum BusRef {
    /// The terminal bus: receives everything, and its output leaves the mix.
    #[default]
    Master,
}

/// What kind of bus a [`Bus`] is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum BusKind {
    /// The terminal bus every track's output lands on.
    #[default]
    Master,
}

/// The channel layout of a bus — what "a position" means, and what the
/// output adapter delivers. v1 is stereo, fixed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum BusLayout {
    /// Left/right. Placement on this layout is a one-dimensional position.
    #[default]
    Stereo,
}

/// The signal path of one track into the mix: its strip (gain, mute,
/// placement) and where the signal goes.
///
/// v1: exactly one output per track, always pointing at the master. When a
/// track may send to several places (an aux), this becomes `Vec<Output>` on
/// the track; when outputs may share one bus strip (a group), the bus gains
/// its own identity.
#[derive(Debug, Clone, PartialEq)]
pub struct Output {
    /// Gain of this output in the mix, `0.0 ..= 1.0`; full by default.
    pub gain: f32,
    /// Whether this output is silent regardless of its gain.
    pub muted: bool,
    /// Where on the destination bus this signal sits.
    pub placement: Placement,
    /// Which bus receives this output.
    pub target: BusRef,
}

impl Default for Output {
    fn default() -> Self {
        Self {
            gain: 1.0,
            muted: false,
            placement: Placement::Stereo { position: 0.0 },
            target: BusRef::Master,
        }
    }
}

impl Output {
    /// Set the output's gain, clamped to `0.0 ..= 1.0`.
    pub fn set_gain(&mut self, gain: f32) {
        self.gain = gain.clamp(0.0, 1.0);
    }

    /// Set the output's placement, clamped to the layout's range.
    pub fn set_placement(&mut self, placement: Placement) {
        self.placement = placement;
    }
}

/// Where a signal sits in the destination bus's space.
///
/// v1's only bus is stereo, so a position is one-dimensional: `-1` hard
/// left, `0` center, `+1` hard right. A stereo signal is balanced (the far
/// side is attenuated, keeping its width); a mono signal is placed with a
/// constant-power law (its energy is shared between the two sides, so it
/// never gets louder or quieter as it moves). Surround layouts would add a
/// variant carrying an azimuth.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Placement {
    /// A position on the stereo field, `-1.0 ..= 1.0`.
    Stereo { position: f32 },
}

impl Default for Placement {
    fn default() -> Self {
        Self::Stereo { position: 0.0 }
    }
}

impl Placement {
    /// The stereo position, `-1.0 ..= 1.0`; clamped on the way in.
    pub fn position(self) -> f32 {
        match self {
            Self::Stereo { position } => position.clamp(-1.0, 1.0),
        }
    }

    /// Set a stereo position, clamped to the layout's range.
    pub fn set_position(&mut self, position: f32) {
        match self {
            Self::Stereo { position: p } => *p = position.clamp(-1.0, 1.0),
        }
    }
}

/// A bus: a summing point in the signal layer.
///
/// v1 has exactly one instance — the master — held by the player. It carries
/// the master gain and the output layout; its output is what the output
/// adapter delivers to a device or a file. A group bus would be another
/// instance whose output points at this one.
#[derive(Debug, Clone, PartialEq)]
pub struct Bus {
    kind: BusKind,
    gain: f32,
    layout: BusLayout,
}

impl Default for Bus {
    fn default() -> Self {
        Self {
            kind: BusKind::Master,
            gain: 1.0,
            layout: BusLayout::Stereo,
        }
    }
}

impl Bus {
    /// The terminal bus: master gain, stereo output.
    #[must_use]
    pub fn master() -> Self {
        Self::default()
    }

    /// What kind of bus this is.
    #[must_use]
    pub fn kind(&self) -> BusKind {
        self.kind
    }

    /// The channel layout of this bus.
    #[must_use]
    pub fn layout(&self) -> BusLayout {
        self.layout
    }

    /// Gain of this bus in the output, `0.0 ..= 1.0`; full by default.
    #[must_use]
    pub fn gain(&self) -> f32 {
        self.gain
    }

    /// Set the bus's gain, clamped to `0.0 ..= 1.0`.
    pub fn set_gain(&mut self, gain: f32) {
        self.gain = gain.clamp(0.0, 1.0);
    }
}
