// Tile coding for function approximation
// Implements hash-based tile coding to handle continuous state spaces efficiently

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

/// Tile coding configuration
pub struct TileCoding {
    num_tilings: usize,     // Number of overlapping tilings (typically 8)
    tiles_per_dim: usize,   // Number of tiles per dimension (typically 4)
    num_dimensions: usize,  // State vector dimensionality
    table_size: usize,      // Hash table size for collision reduction
}

impl Default for TileCoding {
    fn default() -> Self {
        TileCoding::new(8, 4, 25)
    }
}

impl TileCoding {
    pub fn new(num_tilings: usize, tiles_per_dim: usize, num_dimensions: usize) -> Self {
        let table_size = 1 << 18; // 262,144 - large enough to avoid most collisions
        TileCoding {
            num_tilings,
            tiles_per_dim,
            num_dimensions,
            table_size,
        }
    }

    /// Get active tiles for a state vector
    /// Returns a vector of hash indices (one per tiling)
    pub fn get_tiles(&self, state: &[f32], action: u8) -> Vec<usize> {
        let mut tiles = Vec::with_capacity(self.num_tilings);

        for tiling in 0..self.num_tilings {
            let offset = tiling as f32 / self.num_tilings as f32;
            let mut coords = Vec::with_capacity(self.num_dimensions + 1);

            for (dim, &value) in state.iter().enumerate() {
                // Apply offset for this tiling to create overlap
                let shifted = (value + offset).clamp(0.0, 1.0);
                // Discretize into tiles
                let tile_idx = (shifted * self.tiles_per_dim as f32) as usize;
                let tile_idx = tile_idx.min(self.tiles_per_dim - 1);
                coords.push((dim, tile_idx));
            }

            // Include action in the tile hash
            coords.push((self.num_dimensions, action as usize));

            // Hash the coordinates to get table index
            let hash = self.hash_coords(&coords, tiling);
            tiles.push(hash);
        }

        tiles
    }

    /// Hash tile coordinates to a table index
    fn hash_coords(&self, coords: &[(usize, usize)], tiling: usize) -> usize {
        let mut hasher = DefaultHasher::new();

        // Include tiling number in hash to avoid collisions between tilings
        tiling.hash(&mut hasher);

        for &(dim, tile) in coords {
            dim.hash(&mut hasher);
            tile.hash(&mut hasher);
        }

        let hash = hasher.finish();
        (hash as usize) % self.table_size
    }

    pub fn table_size(&self) -> usize {
        self.table_size
    }

    pub fn num_tilings(&self) -> usize {
        self.num_tilings
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_tile_coding_basic() {
        let tc = TileCoding::new(8, 4, 25);
        let state = vec![0.5; 25]; // All features at 0.5
        let tiles = tc.get_tiles(&state, 3);

        assert_eq!(tiles.len(), 8); // One tile per tiling

        // Tiles should be different due to offset
        let unique_tiles: std::collections::HashSet<_> = tiles.iter().collect();
        assert!(unique_tiles.len() > 1, "Tilings should produce different tiles");
    }

    #[test]
    fn test_tile_coding_bounds() {
        let tc = TileCoding::new(8, 4, 25);

        // Test edge cases
        let state_min = vec![0.0; 25];
        let state_max = vec![1.0; 25];

        let tiles_min = tc.get_tiles(&state_min, 0);
        let tiles_max = tc.get_tiles(&state_max, 0);

        assert_eq!(tiles_min.len(), 8);
        assert_eq!(tiles_max.len(), 8);

        // Different states should produce different tiles
        assert_ne!(tiles_min, tiles_max);
    }

    #[test]
    fn test_action_discrimination() {
        let tc = TileCoding::new(8, 4, 25);
        let state = vec![0.5; 25];

        let tiles_action0 = tc.get_tiles(&state, 0);
        let tiles_action1 = tc.get_tiles(&state, 1);

        // Same state, different actions should produce different tiles
        assert_ne!(tiles_action0, tiles_action1);
    }
}
