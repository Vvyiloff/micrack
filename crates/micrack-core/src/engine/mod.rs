//! Audio engine: fixed serial chain mic -> input gain -> slots -> master gain -> output.

pub mod block;
pub mod callback;
pub mod chain;
pub mod drift;

pub use block::{Block, db_to_gain};
pub use callback::AudioProcessor;
pub use chain::{Command, MAX_SLOTS, Slot};