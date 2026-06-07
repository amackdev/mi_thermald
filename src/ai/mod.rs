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

pub use engine::AIEngine;
pub use native::NativeController;
#[allow(unused_imports)]
pub use features::StateVector;
#[allow(unused_imports)]
pub use qtable::QTable;
