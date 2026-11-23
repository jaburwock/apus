//! Samples the alignment strand from the HG002 counts at MAPQ 60.

use rand::{Rng, RngExt};

const FORWARD_COUNT: u32 = 24_655;
const REVERSE_COUNT: u32 = 24_189;

pub fn gen_flag<R: Rng + ?Sized>(rng: &mut R) -> u16 {
    if rng.random_ratio(REVERSE_COUNT, FORWARD_COUNT + REVERSE_COUNT) {
        16
    } else {
        0
    }
}
