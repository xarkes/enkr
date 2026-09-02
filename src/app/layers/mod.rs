//! Transient surfaces layered above the current [`View`](super::View).
//!
//! Painted in a fixed order — menu, then palette, then modal — so z-order is
//! correct by construction rather than by call-site discipline. Note that mae
//! overlays do not block each other's *input* (only non-overlay boxes below
//! them), so opening a higher layer must actively close the lower ones; that
//! invariant lives with the state machine, not here.

pub(crate) mod menu;
pub(crate) mod modal;
pub(crate) mod palette;

pub(crate) use menu::*;
pub(crate) use modal::*;
pub(crate) use palette::*;
