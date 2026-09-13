// AI thermal management module
// Implements Q-learning with tile coding for adaptive thermal control

mod tile_coding;
mod qtable;
mod features;
mod rewards;
mod safety;
mod data_collector;
mod engine;
mod native;
mod workload;

pub use engine::AIEngine;
pub use native::NativeController;
pub use workload::WorkloadMode;

/// Linearly interpolate across the 10 discrete actions (0..=9) into
/// `base..=base+span`. `ascending=true` maps action 0 -> base, 9 -> base+span
/// (e.g. cooling channel min -> max); `ascending=false` maps action 0 ->
/// base+span, 9 -> base (e.g. thermal level max -> 0, most-cooling first).
/// Values out of the 0..=9 range are not expected but are clamped.
pub(crate) fn lerp_by_action(action: u8, base: i32, span: i32, ascending: bool) -> i32 {
    let action = action.min(9) as i32;
    let step = if ascending { action } else { 9 - action };
    base + span * step / 9
}
