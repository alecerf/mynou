//! SHA-1 and SHA-256 block compression with the Armv8 cryptographic extension.
//!
//! This is the only module that may contain `unsafe`: one block per entry
//! point, to call a `#[target_feature]` function. The module is compiled only
//! when the build itself enables `neon` and `sha2` (true for every
//! aarch64-apple-darwin target), so any CPU that runs the binary has the
//! instructions; the intrinsics inside take no pointers and are safe to call.
//! The portable compressors in hashes.rs stay the reference that tests compare
//! these against.

use std::arch::aarch64::{
    uint32x4_t, vaddq_u32, vdupq_n_u32, vgetq_lane_u32, vsetq_lane_u32, vsha1cq_u32, vsha1h_u32,
    vsha1mq_u32, vsha1pq_u32, vsha1su0q_u32, vsha1su1q_u32, vsha256h2q_u32, vsha256hq_u32,
    vsha256su0q_u32, vsha256su1q_u32,
};

/// Compress whole SHA-1 blocks into `state`.
pub(super) fn sha1_blocks(state: &mut [u32; 5], blocks: &[[u8; 64]]) {
    // SAFETY: the module is only compiled when the build enables neon and sha2.
    unsafe { sha1(state, blocks) }
}

/// Compress whole SHA-256 blocks into `state`.
pub(super) fn sha256_blocks(state: &mut [u32; 8], blocks: &[[u8; 64]]) {
    // SAFETY: the module is only compiled when the build enables neon and sha2.
    unsafe { sha256(state, blocks) }
}

#[inline]
#[target_feature(enable = "neon")]
fn vector([a, b, c, d]: [u32; 4]) -> uint32x4_t {
    let v = vdupq_n_u32(a);
    let v = vsetq_lane_u32::<1>(b, v);
    let v = vsetq_lane_u32::<2>(c, v);
    vsetq_lane_u32::<3>(d, v)
}

#[inline]
#[target_feature(enable = "neon")]
fn words(v: uint32x4_t) -> [u32; 4] {
    [
        vgetq_lane_u32::<0>(v),
        vgetq_lane_u32::<1>(v),
        vgetq_lane_u32::<2>(v),
        vgetq_lane_u32::<3>(v),
    ]
}

/// The block as four big-endian message vectors.
#[inline]
#[target_feature(enable = "neon")]
fn message(block: &[u8; 64]) -> [uint32x4_t; 4] {
    let words = block.as_chunks::<4>().0;
    let lane = |i: usize| u32::from_be_bytes(words[i]);
    [
        vector([lane(0), lane(1), lane(2), lane(3)]),
        vector([lane(4), lane(5), lane(6), lane(7)]),
        vector([lane(8), lane(9), lane(10), lane(11)]),
        vector([lane(12), lane(13), lane(14), lane(15)]),
    ]
}

#[target_feature(enable = "neon,sha2")]
fn sha1(state: &mut [u32; 5], blocks: &[[u8; 64]]) {
    let [a, b, c, d, mut e] = *state;
    let mut abcd = vector([a, b, c, d]);
    for block in blocks {
        let (saved_abcd, saved_e) = (abcd, e);
        let mut w = message(block);
        // Each step runs four rounds on one message vector, then extends the
        // schedule by the vector four steps ahead (W[i] from W[i-16..i-1]).
        macro_rules! steps {
            ($round:ident, $k:literal; $($i:literal),+) => {$(
                let wk = vaddq_u32(w[$i % 4], vdupq_n_u32($k));
                let next_e = vsha1h_u32(vgetq_lane_u32::<0>(abcd));
                abcd = $round(abcd, e, wk);
                e = next_e;
                if $i < 16 {
                    w[$i % 4] = vsha1su1q_u32(
                        vsha1su0q_u32(w[$i % 4], w[($i + 1) % 4], w[($i + 2) % 4]),
                        w[($i + 3) % 4],
                    );
                }
            )+};
        }
        steps!(vsha1cq_u32, 0x5a827999; 0, 1, 2, 3, 4);
        steps!(vsha1pq_u32, 0x6ed9eba1; 5, 6, 7, 8, 9);
        steps!(vsha1mq_u32, 0x8f1bbcdc; 10, 11, 12, 13, 14);
        steps!(vsha1pq_u32, 0xca62c1d6; 15, 16, 17, 18, 19);
        abcd = vaddq_u32(abcd, saved_abcd);
        e = e.wrapping_add(saved_e);
    }
    let [a, b, c, d] = words(abcd);
    *state = [a, b, c, d, e];
}

#[target_feature(enable = "neon,sha2")]
fn sha256(state: &mut [u32; 8], blocks: &[[u8; 64]]) {
    let k = super::hashes::SHA256_K.as_chunks::<4>().0;
    let [a, b, c, d, e, f, g, h] = *state;
    let (mut abcd, mut efgh) = (vector([a, b, c, d]), vector([e, f, g, h]));
    for block in blocks {
        let (saved_abcd, saved_efgh) = (abcd, efgh);
        let mut w = message(block);
        // Each step runs four rounds, then extends the schedule as in sha1.
        macro_rules! steps {
            ($($i:literal),+) => {$(
                let wk = vaddq_u32(w[$i % 4], vector(k[$i]));
                let previous = abcd;
                abcd = vsha256hq_u32(abcd, efgh, wk);
                efgh = vsha256h2q_u32(efgh, previous, wk);
                if $i < 12 {
                    w[$i % 4] = vsha256su1q_u32(
                        vsha256su0q_u32(w[$i % 4], w[($i + 1) % 4]),
                        w[($i + 2) % 4],
                        w[($i + 3) % 4],
                    );
                }
            )+};
        }
        steps!(0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15);
        abcd = vaddq_u32(abcd, saved_abcd);
        efgh = vaddq_u32(efgh, saved_efgh);
    }
    let ([a, b, c, d], [e, f, g, h]) = (words(abcd), words(efgh));
    *state = [a, b, c, d, e, f, g, h];
}
