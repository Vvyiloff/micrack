//! Chain types: slots and UI->audio commands. No locking; commands travel via rtrb.

use crate::plugins::PluginInstance;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

pub const MAX_SLOTS: usize = 32;

static NEXT_SLOT_ID: AtomicU64 = AtomicU64::new(1);

pub struct Slot {
    pub id: u64,
    /// Shared with the control-thread registry and any open editor. The audio
    /// thread locks it to process; the control thread locks the SAME mutex to
    /// open/close/idle the editor, so processing continues while editing.
    pub instance: Arc<Mutex<PluginInstance>>,
    pub bypass: bool,
    /// Current wet gain 0..1, ramped per block to avoid clicks on bypass/insert.
    pub fade: f32,
}

impl Slot {
    pub fn new(instance: Arc<Mutex<PluginInstance>>) -> Self {
        Self { id: NEXT_SLOT_ID.fetch_add(1, Ordering::Relaxed), instance, bypass: false, fade: 0.0 }
    }
}

/// UI -> audio thread commands (rtrb). Applied at the top of each output block.
/// Slots travel BY VALUE both ways: boxing/unboxing would alloc/dealloc on the
/// audio thread (M3).
pub enum Command {
    Insert(usize, Slot),
    Remove(usize),
    Move(usize, usize),
    SetBypass(usize, bool),
    /// Parameter write applied ON the audio thread (N1): the control thread no
    /// longer locks the instance for tuning, so agent set_parameter storms never
    /// contend with the callback. Addressed by stable slot id, not index.
    SetParam(u64, u32, f64),
    Shutdown,
}