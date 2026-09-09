//! Shared types and pure logic for the telemouse pipeline.
//!
//! Everything here is platform-independent and allocation-conscious. The
//! capture agent, viz bridge, and analysis tools all speak these types; the
//! wire format is JSON (see [`wire::Envelope`]) for now, per the plan's
//! "start with JSON for debuggability" decision.
//!
//! Design rule: raw HID counts stay raw on the wire. Physical units (cm,
//! aim-space degrees) are derived in consumers via [`units`] and the
//! per-session [`session::SessionConfig`].

pub mod batch;
pub mod batcher;
pub mod clock;
pub mod config;
pub mod event;
pub mod hotkey;
pub mod localhost;
pub mod panic_hook;
pub mod recordings;
pub mod session;
pub mod units;
pub mod wire;

pub use batch::{Batch, BatchView};
pub use batcher::Batcher;
pub use clock::{QpcAnchor, now_utc_us};
pub use event::{RawEvent, buttons};
pub use session::{GameSens, Marker, MonitorInfo, SessionConfig};
pub use wire::{Envelope, EnvelopeView};
