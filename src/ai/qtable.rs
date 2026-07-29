use serde::{Deserialize, Serialize};

use super::tile_coding::TileCoding;

const NUM_ACTIONS: u8 = 10;
const LEARNING_RATE: f32 = 0.1;
const DISCOUNT_FACTOR: f32 = 0.95;
const EPSILON_START: f32 = 0.3;
const EPSILON_MIN: f32 = 0.05;
const EPSILON_DECAY_TICKS: u64 = 604_800;

#[derive(Serialize, Deserialize)]
pub struct QTable {
    weights: Vec<f32>,
    #[serde(skip)]
    tile_coding: TileCoding,
    epsilon: f32,
    tick_count: u64,
}

impl QTable {
    pub fn new() -> Self {
        let tile_coding = TileCoding::new(8, 4, 27);
        let table_size = tile_coding.table_size();
        QTable {
            weights: vec![0.0; table_size],
            tile_coding,
            epsilon: EPSILON_START,
            tick_count: 0,
        }
    }

    pub fn select_action(&mut self, state: &[f32]) -> u8 {
        self.tick_count += 1;
        self.decay_epsilon();

        if fastrand::f32() < self.epsilon {
            fastrand::u8(0..NUM_ACTIONS)
        } else {
            self.greedy_action(state)
        }
    }

    pub fn greedy_action(&self, state: &[f32]) -> u8 {
        let mut best_actions = Vec::new();
        let mut best_value = f32::NEG_INFINITY;

        for action in 0..NUM_ACTIONS {
            let q = self.get_q_value(state, action);
            if q > best_value {
                best_value = q;
                best_actions.clear();
                best_actions.push(action);
            } else if (q - best_value).abs() < 1e-9 {
                best_actions.push(action);
            }
        }

        best_actions[fastrand::usize(0..best_actions.len())]
    }

    pub fn get_q_value(&self, state: &[f32], action: u8) -> f32 {
        let tiles = self.tile_coding.get_tiles(state, action);
        tiles.iter().map(|&tile| self.weights[tile]).sum()
    }

    pub fn update(&mut self, state: &[f32], action: u8, reward: f32, next_state: &[f32]) {
        let current_q = self.get_q_value(state, action);
        let max_next_q = self.best_q_value(next_state);
        let target = reward + DISCOUNT_FACTOR * max_next_q;
        let td_error = target - current_q;

        let tiles = self.tile_coding.get_tiles(state, action);
        let update = LEARNING_RATE * td_error / self.tile_coding.num_tilings() as f32;

        for &tile in &tiles {
            self.weights[tile] += update;
        }
    }

    fn best_q_value(&self, state: &[f32]) -> f32 {
        let mut best = f32::NEG_INFINITY;
        for action in 0..NUM_ACTIONS {
            let q = self.get_q_value(state, action);
            if q > best {
                best = q;
            }
        }
        best
    }

    fn decay_epsilon(&mut self) {
        let decay = (self.tick_count as f32 / EPSILON_DECAY_TICKS as f32).min(1.0);
        self.epsilon = EPSILON_START + (EPSILON_MIN - EPSILON_START) * decay;
    }

    pub fn epsilon(&self) -> f32 {
        self.epsilon
    }

    pub fn weights(&self) -> &[f32] {
        &self.weights
    }

    pub fn set_weights(&mut self, weights: &[f32]) -> Option<()> {
        if weights.len() != self.weights.len() {
            return None;
        }
        self.weights.copy_from_slice(weights);
        Some(())
    }
}
