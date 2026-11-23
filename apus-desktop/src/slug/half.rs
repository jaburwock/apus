//! IEEE 754 binary16 (half-float) conversions for Slug curve control points.
//!
//! The curve texture is `Rgba16Float`, so every control point is stored as a half float.
//! Band assignment must run on the exact half-rounded coordinates the GPU will read back,
//! otherwise a curve sitting right on a band boundary can round across it on upload.
//!
//! Bit-level conversions follow Fabian Giesen's public-domain float-to-half (round to
//! nearest, ties to even) and exact half-to-float algorithms. See
//! <https://fgiesen.wordpress.com/2012/03/28/half-to-float-done-quic/>.

/// Converts an `f32` to IEEE 754 binary16 bits (round to nearest, ties to even).
pub fn to_half(value: f32) -> u16 {
    const INFINITY: u32 = 255 << 23;
    const HALF_OVERFLOW: u32 = (127 + 16) << 23;
    const SMALLEST_HALF_NORMAL: u32 = 113 << 23;
    const SUBNORMAL_MAGIC: u32 = ((127 - 15) + (23 - 10) + 1) << 23;

    let mut bits = value.to_bits();
    let sign = bits & 0x8000_0000;
    bits ^= sign;

    let half = if bits >= HALF_OVERFLOW {
        if bits > INFINITY { 0x7e00 } else { 0x7c00 }
    } else if bits < SMALLEST_HALF_NORMAL {
        // Aligns the ten mantissa bits at the bottom so FPU round-to-nearest-even rounds them.
        let aligned = f32::from_bits(bits) + f32::from_bits(SUBNORMAL_MAGIC);
        aligned.to_bits() - SUBNORMAL_MAGIC
    } else {
        let mantissa_odd = (bits >> 13) & 1;
        bits = bits.wrapping_add(((15u32.wrapping_sub(127)) << 23) + 0x0fff + mantissa_odd);
        bits >> 13
    };

    (half | (sign >> 16)) as u16
}

/// Converts IEEE 754 binary16 bits to an exact `f32`.
pub fn from_half(half: u16) -> f32 {
    const EXPONENT_MASK: u32 = 0x7c00 << 13;
    const SUBNORMAL_MAGIC: u32 = 113 << 23;

    let mut bits = u32::from(half & 0x7fff) << 13;
    let exponent = bits & EXPONENT_MASK;
    bits += (127 - 15) << 23;

    if exponent == EXPONENT_MASK {
        bits += (128 - 16) << 23;
    } else if exponent == 0 {
        bits += 1 << 23;
        bits = (f32::from_bits(bits) - f32::from_bits(SUBNORMAL_MAGIC)).to_bits();
    }

    f32::from_bits(bits | (u32::from(half & 0x8000) << 16))
}

/// Rounds `value` to the nearest representable IEEE binary16 value.
pub fn to_half_precision(value: f32) -> f32 {
    from_half(to_half(value))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn half_round_trips_every_finite_bit_pattern() {
        for raw in 0u16..=u16::MAX {
            let exp = (raw >> 10) & 0x1f;
            if exp == 0x1f {
                continue; // NaN / Inf
            }
            let f = from_half(raw);
            let back = to_half(f);
            assert_eq!(back, raw, "bit pattern 0x{raw:04x} ({f}) did not round-trip");
        }
    }

    #[test]
    fn half_handles_special_values_and_ties_to_even() {
        assert_eq!(to_half(0.0), 0x0000);
        assert_eq!(to_half(-0.0), 0x8000);
        assert_eq!(to_half(1.0), 0x3c00);
        assert_eq!(to_half(-2.0), 0xc000);
        assert_eq!(to_half(f32::INFINITY), 0x7c00);
        assert_eq!(to_half(f32::NEG_INFINITY), 0xfc00);
        assert_eq!(to_half(f32::NAN) & 0x7fff, 0x7e00);

        // Exact halfway between 1.0 (0x3c00, even) and 1.0 + 2^-10 (0x3c01, odd) rounds to even.
        let ulp = 1.0 / 1024.0;
        assert_eq!(to_half(1.0 + 0.5 * ulp), 0x3c00);
        assert_eq!(to_half(1.0 + 1.5 * ulp), 0x3c02);

        // Quantization is idempotent.
        for sample in [-1.23456f32, -0.000_05, 0.0, 0.333_333_34, 1.0 / 2048.0, 42.125] {
            let q = to_half_precision(sample);
            assert_eq!(to_half_precision(q), q);
        }
    }
}
