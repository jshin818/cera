//! Frecency: exponentially decayed access counts. score = score * 0.5^((now-last)/HALF_LIFE) + weight.

use serde::{Deserialize, Serialize};

pub const HALF_LIFE_MS: f64 = 7.0 * 24.0 * 3600.0 * 1000.0;

#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct Frecency {
    pub score: f32,
    pub last_access_ms: i64,
}

impl Frecency {
    /// weight: open = 1, edit = 2.
    pub fn bump(&mut self, now_ms: i64, weight: f32) {
        self.score = self.decayed(now_ms) + weight;
        self.last_access_ms = now_ms;
    }

    pub fn decayed(&self, now_ms: i64) -> f32 {
        if self.score == 0.0 {
            return 0.0;
        }
        let dt = (now_ms - self.last_access_ms).max(0) as f64;
        (self.score as f64 * 0.5f64.powf(dt / HALF_LIFE_MS)) as f32
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn halves_after_half_life() {
        let mut f = Frecency::default();
        f.bump(0, 2.0);
        assert!((f.decayed(HALF_LIFE_MS as i64) - 1.0).abs() < 1e-4);
        f.bump(HALF_LIFE_MS as i64, 1.0);
        assert!((f.score - 2.0).abs() < 1e-4);
    }
}
