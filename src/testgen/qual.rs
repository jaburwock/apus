//! Generates SAM QUAL strings with a bounded, mean-reverting per-base walk.
//!
//! The walk keeps nearby bases correlated around the HG002 mean within the SAM Phred range.

use rand::{Rng, RngExt};

const MEAN_QUAL: f64 = 35_807_523_933.0 / 917_536_454.0;

/// Maximum per-base walk step.
const MAX_STEP: i32 = 2;

/// Valid SAM Phred range.
const MIN_Q: i32 = 0;
const MAX_Q: i32 = 93;

/// Generates a mean-reverting QUAL string.
pub fn gen_qual<R: Rng + ?Sized>(rng: &mut R, read_len: u64) -> String {
    let mut qual = rng.random_range((MEAN_QUAL.round() as i32 - 1)..=(MEAN_QUAL.round() as i32 + 1));

    let mut quals = Vec::with_capacity(read_len as usize);
    for _ in 0..read_len {
        quals.push(qual);

        let drift = MEAN_QUAL - qual as f64;
        let bias = (drift / 10.0).clamp(-0.9, 0.9);
        let step = weighted_step(rng, bias);
        qual = (qual + step).clamp(MIN_Q, MAX_Q);
    }

    quals.into_iter().map(|q| (33 + q) as u8 as char).collect()
}

/// Samples a bounded step with the supplied directional bias.
fn weighted_step<R: Rng + ?Sized>(rng: &mut R, bias: f64) -> i32 {
    let step_magnitude = rng.random_range(0..=MAX_STEP);
    if step_magnitude == 0 {
        return 0;
    }
    let positive_prob = 0.5 + bias / 2.0;
    if rng.random_bool(positive_prob) {
        step_magnitude
    } else {
        -step_magnitude
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand::SeedableRng;
    use rand::rngs::StdRng;

    #[test]
    fn generated_qualities_match_read_length_and_phred_contract() {
        let mut rng = StdRng::seed_from_u64(5);
        let read_len = 1_024;
        let qual = gen_qual(&mut rng, read_len);
        assert_eq!(qual.chars().count(), read_len as usize);
        for c in qual.chars() {
            let q = c as i32 - 33;
            assert!((MIN_Q..=MAX_Q).contains(&q), "quality {q} out of Phred range");
        }
        let mean: f64 = qual.chars().map(|c| (c as i32 - 33) as f64).sum::<f64>() / qual.chars().count() as f64;
        assert!(
            (mean - MEAN_QUAL).abs() < 2.0,
            "mean {mean} drifted too far from target {MEAN_QUAL}"
        );
    }
}
