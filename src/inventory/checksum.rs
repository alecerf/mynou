//! XXH64, the inventory's corruption checksum.
//!
//! Inventory checksums detect storage faults, not malicious rewrites (see
//! docs/inventory-storage.md), so they need no cryptographic strength. XXH64
//! reads 32 bytes per round in four independent lanes, which keeps it far
//! below the I/O cost of the bytes it covers.

const P1: u64 = 0x9E37_79B1_85EB_CA87;
const P2: u64 = 0xC2B2_AE3D_27D4_EB4F;
const P3: u64 = 0x1656_67B1_9E37_79F9;
const P4: u64 = 0x85EB_CA77_C2B2_AE63;
const P5: u64 = 0x27D4_EB2F_1656_67C5;

fn round(accumulator: u64, input: u64) -> u64 {
    accumulator
        .wrapping_add(input.wrapping_mul(P2))
        .rotate_left(31)
        .wrapping_mul(P1)
}

fn merge(accumulator: u64, lane: u64) -> u64 {
    (accumulator ^ round(0, lane))
        .wrapping_mul(P1)
        .wrapping_add(P4)
}

pub(super) fn xxh64(data: &[u8]) -> u64 {
    let mut rest = data;
    let mut hash = if data.len() >= 32 {
        let mut lanes = [P1.wrapping_add(P2), P2, 0, 0u64.wrapping_sub(P1)];
        let (stripes, remainder) = rest.as_chunks::<32>();
        for stripe in stripes {
            for (lane, bytes) in lanes.iter_mut().zip(stripe.as_chunks::<8>().0) {
                *lane = round(*lane, u64::from_le_bytes(*bytes));
            }
        }
        rest = remainder;
        let [a, b, c, d] = lanes;
        let hash = a
            .rotate_left(1)
            .wrapping_add(b.rotate_left(7))
            .wrapping_add(c.rotate_left(12))
            .wrapping_add(d.rotate_left(18));
        lanes.iter().fold(hash, |hash, lane| merge(hash, *lane))
    } else {
        P5
    };
    hash = hash.wrapping_add(data.len() as u64);
    let (words, remainder) = rest.as_chunks::<8>();
    for bytes in words {
        hash = (hash ^ round(0, u64::from_le_bytes(*bytes)))
            .rotate_left(27)
            .wrapping_mul(P1)
            .wrapping_add(P4);
    }
    rest = remainder;
    if rest.len() >= 4 {
        let mut half = [0; 4];
        half.copy_from_slice(&rest[..4]);
        hash = (hash ^ u64::from(u32::from_le_bytes(half)).wrapping_mul(P1))
            .rotate_left(23)
            .wrapping_mul(P2)
            .wrapping_add(P3);
        rest = &rest[4..];
    }
    for byte in rest {
        hash = (hash ^ u64::from(*byte).wrapping_mul(P5))
            .rotate_left(11)
            .wrapping_mul(P1);
    }
    hash ^= hash >> 33;
    hash = hash.wrapping_mul(P2);
    hash ^= hash >> 29;
    hash = hash.wrapping_mul(P3);
    hash ^ (hash >> 32)
}

#[cfg(test)]
mod tests {
    use super::xxh64;

    /// Reference values from the xxHash library (python-xxhash 4.0.2).
    #[test]
    fn matches_the_reference_implementation_at_every_tail_length() {
        let data: Vec<u8> = (0..5000u32).map(|i| (i * 7 + 3) as u8).collect();
        for (length, expected) in [
            (0, 0xef46db3751d8e999),
            (1, 0x1f25c8d0bc1f4bb6),
            (3, 0x31d2363f52e564c9),
            (4, 0x9bb64b7d66ee9fda),
            (5, 0xc7608efddb7051fe),
            (7, 0x9a7b149959ce60d8),
            (8, 0xdab99d95c6f90092),
            (9, 0x170bb6bf975b4c02),
            (15, 0x1b47cb8243cc8e32),
            (16, 0x434850232b787be2),
            (31, 0xa2aa5f33cc4a6119),
            (32, 0x23c3c17ef790fd97),
            (33, 0x50a7cfc7ba588784),
            (63, 0x5e3e54b431c7493c),
            (64, 0x0eb64b3ef6eeb01f),
            (100, 0xa61f8d4c170fe531),
            (4064, 0xd5f06e1b888f6ef8),
            (5000, 0x9a8d831c04aa8b1d),
        ] {
            assert_eq!(xxh64(&data[..length]), expected, "length {length}");
        }
        assert_eq!(xxh64(b"abc"), 0x44bc2cf5ad770999);
    }
}
