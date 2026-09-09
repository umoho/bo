//! The signal layer: a track's output edge ([`Output`]) and the bus nodes it
//! feeds into — the master ([`Bus`]) and the group buses ([`Group`]).
//!
//! Kept apart from the content layer ([`crate::track`]) on purpose: a
//! [`Track`](crate::track::Track) there is a container of clips on a
//! timeline — *what* plays, *when* — and nothing else; this module is *where
//! the sound goes*. A track reaches into the signal layer through exactly one
//! edge, its output, which carries the strip (gain, mute, placement) and
//! points at a bus. The master receives everything and its output leaves the
//! mix; a group bus — shown to users as a plain *bus* — is a summing point
//! several outputs can be routed to instead, so one strip (its gain and
//! mute) can duck a whole set of tracks together, and the sum feeds the
//! master. A layout beyond stereo is where 5.1/7.1 would land; it does not
//! exist yet, and the enums keep their seams.

/// Where an output lands: which bus receives its signal.
///
/// Every output points at one of two things: the master, or a group bus
/// addressed by its stable id (see [`Group`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub enum BusRef {
    /// The terminal bus: receives everything, and its output leaves the mix.
    #[default]
    Master,
    /// A group bus, by the stable id it was created with.
    Group(u64),
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
/// v1: exactly one output per track, pointing at the master or a group bus.
/// When a track may send to several places (an aux), this becomes
/// `Vec<Output>` on the track.
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

/// A group bus: a summing point whose output feeds the master.
///
/// Several tracks' outputs can be routed here, and their signals sum to one
/// stereo pair that carries this strip — its gain and mute — before the
/// master hears it. That is what lets one knob duck a whole set of tracks
/// together without touching the tracks' own strips (a radio "music bus" or
/// "voice bus"). Shown to users as a plain *bus*; the model calls it a group
/// to keep the terminal master ([`Bus`]) apart.
///
/// v1: every group feeds the master directly (no group of groups), carries
/// no placement of its own, and is addressed by the stable id it was created
/// with, as [`BusRef::Group`].
#[derive(Debug, Clone, PartialEq)]
pub struct Group {
    id: u64,
    name: Option<String>,
    gain: f32,
    muted: bool,
}

impl Group {
    /// A fresh group with this stable id, at full gain and audible.
    #[must_use]
    pub fn new(id: u64) -> Self {
        Self {
            id,
            name: None,
            gain: 1.0,
            muted: false,
        }
    }

    /// The stable id [`BusRef::Group`] addresses this group by.
    #[must_use]
    pub fn id(&self) -> u64 {
        self.id
    }

    /// Label, if set. Names are for humans and agents; the id is identity.
    #[must_use]
    pub fn name(&self) -> Option<&str> {
        self.name.as_deref()
    }

    /// Relabel the group.
    pub fn set_name(&mut self, name: impl Into<String>) {
        self.name = Some(name.into());
    }

    /// Gain of this group's output into the master, `0.0 ..= 1.0`; full by
    /// default.
    #[must_use]
    pub fn gain(&self) -> f32 {
        self.gain
    }

    /// Set the group's gain, clamped to `0.0 ..= 1.0`.
    pub fn set_gain(&mut self, gain: f32) {
        self.gain = gain.clamp(0.0, 1.0);
    }

    /// Whether the group is muted in the mix. A muted group contributes
    /// nothing to the master regardless of its gain.
    #[must_use]
    pub fn muted(&self) -> bool {
        self.muted
    }

    /// Mute or unmute the group.
    pub fn set_muted(&mut self, muted: bool) {
        self.muted = muted;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn outputs_default_to_the_master() {
        assert_eq!(Output::default().target, BusRef::Master);
        assert_eq!(BusRef::default(), BusRef::Master);
    }

    #[test]
    fn group_refs_are_copyable_and_compare() {
        let a = BusRef::Group(1);
        let b = BusRef::Group(1);
        let c = BusRef::Group(2);
        assert_eq!(a, b);
        assert_ne!(a, c);
        assert_ne!(a, BusRef::Master);
        let d = a; // Copy: taking it twice does not move it.
        assert_eq!(d, a);
    }

    #[test]
    fn a_group_carries_its_strip_and_keeps_its_id() {
        let mut g = Group::new(3);
        assert_eq!(g.id(), 3);
        assert!(g.name().is_none());
        assert_eq!(g.gain(), 1.0);
        assert!(!g.muted());

        g.set_name("music");
        assert_eq!(g.name(), Some("music"));
        g.set_gain(0.4);
        assert_eq!(g.gain(), 0.4);
        g.set_muted(true);
        assert!(g.muted());
        g.set_muted(false);
        assert!(!g.muted());
        assert_eq!(g.id(), 3, "the id never moves");
    }

    #[test]
    fn a_group_gain_clamps_like_every_other_strip() {
        let mut g = Group::new(0);
        g.set_gain(2.0);
        assert_eq!(g.gain(), 1.0);
        g.set_gain(-1.0);
        assert_eq!(g.gain(), 0.0);
    }
}
