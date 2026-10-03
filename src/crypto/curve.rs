//! Montgomery-ladder X25519, RFC 7748. Field limbs use radix 2^51.

use crate::Result;

const MASK: u64 = (1u64 << 51) - 1;

#[derive(Clone, Copy)]
struct Field([u64; 5]);

impl Field {
    const ZERO: Self = Self([0; 5]);
    const ONE: Self = Self([1, 0, 0, 0, 0]);

    fn decode(mut bytes: [u8; 32]) -> Self {
        // RFC 7748 requires accepting non-canonical inputs and ignoring bit 255.
        bytes[31] &= 127;
        let mut limbs = [0u64; 5];
        for bit in 0..255 {
            limbs[bit / 51] |= u64::from((bytes[bit / 8] >> (bit % 8)) & 1) << (bit % 51);
        }
        Self(limbs)
    }

    fn reduce(mut limbs: [u128; 5]) -> Self {
        // Three fixed carry passes cover all bounds produced by add/sub/mul.
        for _ in 0..3 {
            for i in 0..4 {
                let carry = limbs[i] >> 51;
                limbs[i] &= u128::from(MASK);
                limbs[i + 1] += carry;
            }
            let carry = limbs[4] >> 51;
            limbs[4] &= u128::from(MASK);
            limbs[0] += carry * 19;
        }
        Self(limbs.map(|x| x as u64))
    }

    fn encode(self) -> [u8; 32] {
        let limbs = Self::reduce(self.0.map(u128::from)).0;
        // h + 19 overflows 2^255 exactly when h >= 2^255 - 19.
        let mut candidate = limbs;
        candidate[0] += 19;
        for i in 0..4 {
            let carry = candidate[i] >> 51;
            candidate[i] &= MASK;
            candidate[i + 1] += carry;
        }
        let overflow = candidate[4] >> 51;
        candidate[4] &= MASK;
        let select = 0u64.wrapping_sub(overflow);
        let canonical =
            std::array::from_fn::<_, 5, _>(|i| (limbs[i] & !select) | (candidate[i] & select));
        let mut output = [0u8; 32];
        for bit in 0..255 {
            output[bit / 8] |= (((canonical[bit / 51] >> (bit % 51)) & 1) as u8) << (bit % 8);
        }
        output
    }

    fn add(self, rhs: Self) -> Self {
        Self::reduce(std::array::from_fn(|i| {
            u128::from(self.0[i]) + u128::from(rhs.0[i])
        }))
    }

    fn sub(self, rhs: Self) -> Self {
        let twice_modulus = [
            (1u128 << 52) - 38,
            (1u128 << 52) - 2,
            (1u128 << 52) - 2,
            (1u128 << 52) - 2,
            (1u128 << 52) - 2,
        ];
        Self::reduce(std::array::from_fn(|i| {
            u128::from(self.0[i]) + twice_modulus[i] - u128::from(rhs.0[i])
        }))
    }

    fn mul(self, rhs: Self) -> Self {
        let a = self.0.map(u128::from);
        let b = rhs.0.map(u128::from);
        let mut product = [0u128; 5];
        for (i, &a_limb) in a.iter().enumerate() {
            for (j, &b_limb) in b.iter().enumerate() {
                let degree = i + j;
                product[degree % 5] += a_limb * b_limb * if degree >= 5 { 19 } else { 1 };
            }
        }
        Self::reduce(product)
    }

    fn square(self) -> Self {
        self.mul(self)
    }
    fn mul_small(self, rhs: u64) -> Self {
        Self::reduce(self.0.map(|x| u128::from(x) * u128::from(rhs)))
    }

    fn invert(self) -> Self {
        // Fermat: z^(p-2). Exponent bits are fixed and public.
        let mut exponent = [0xffu8; 32];
        exponent[0] = 0xeb;
        exponent[31] = 0x7f;
        let mut power = Self::ONE;
        for bit in (0..255).rev() {
            power = power.square();
            if (exponent[bit / 8] >> (bit % 8)) & 1 != 0 {
                power = power.mul(self);
            }
        }
        power
    }

    fn swap(a: &mut Self, b: &mut Self, bit: u64) {
        let mask = 0u64.wrapping_sub(bit);
        for i in 0..5 {
            let diff = mask & (a.0[i] ^ b.0[i]);
            a.0[i] ^= diff;
            b.0[i] ^= diff;
        }
    }
}

fn ladder(mut scalar: [u8; 32], coordinate: [u8; 32]) -> [u8; 32] {
    scalar[0] &= 248;
    scalar[31] &= 127;
    scalar[31] |= 64;
    let x1 = Field::decode(coordinate);
    let mut x2 = Field::ONE;
    let mut z2 = Field::ZERO;
    let mut x3 = x1;
    let mut z3 = Field::ONE;
    let mut swap = 0u64;
    for bit in (0..255).rev() {
        let k = u64::from((scalar[bit / 8] >> (bit % 8)) & 1);
        swap ^= k;
        Field::swap(&mut x2, &mut x3, swap);
        Field::swap(&mut z2, &mut z3, swap);
        swap = k;
        let a = x2.add(z2);
        let aa = a.square();
        let b = x2.sub(z2);
        let bb = b.square();
        let e = aa.sub(bb);
        let c = x3.add(z3);
        let d = x3.sub(z3);
        let da = d.mul(a);
        let cb = c.mul(b);
        x3 = da.add(cb).square();
        z3 = x1.mul(da.sub(cb).square());
        x2 = aa.mul(bb);
        z2 = e.mul(aa.add(e.mul_small(121665)));
    }
    Field::swap(&mut x2, &mut x3, swap);
    Field::swap(&mut z2, &mut z3, swap);
    x2.mul(z2.invert()).encode()
}

/// Compute an X25519 public key from private random bytes (clamped internally).
pub fn x25519_public_key(secret: [u8; 32]) -> [u8; 32] {
    let mut base = [0u8; 32];
    base[0] = 9;
    ladder(secret, base)
}

/// Compute an X25519 shared secret, rejecting the all-zero low-order result.
pub fn x25519(secret: [u8; 32], peer: [u8; 32]) -> Result<[u8; 32]> {
    let shared = ladder(secret, peer);
    if super::constant_time_eq(&shared, &[0u8; 32]) {
        Err("X25519 : clé de pair de petit ordre refusée".into())
    } else {
        Ok(shared)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crypto::tests::bytes;
    fn array(hex: &str) -> [u8; 32] {
        bytes(hex).try_into().unwrap()
    }
    #[test]
    fn rfc7748_scalar_vectors() {
        let vectors = [
            (
                "a546e36bf0527c9d3b16154b82465edd62144c0ac1fc5a18506a2244ba449ac4",
                "e6db6867583030db3594c1a424b15f7c726624ec26b3353b10a903a6d0ab1c4c",
                "c3da55379de9c6908e94ea4df28d084f32eccf03491c71f754b4075577a28552",
            ),
            (
                "4b66e9d4d1b4673c5ad22691957d6af5c11b6421e0ea01d42ca4169e7918ba0d",
                "e5210f12786811d3f4b7959d0538ae2c31dbe7106fc03c3efc4cd549c715a493",
                "95cbde9476e8907d7aade45cb4b873f88b595a68799fa152e6f8f7647aac7957",
            ),
        ];
        for (scalar, coordinate, expected) in vectors {
            assert_eq!(
                x25519(array(scalar), array(coordinate)).unwrap(),
                array(expected)
            );
        }
    }
    #[test]
    fn rfc7748_key_exchange() {
        let alice = array("77076d0a7318a57d3c16c17251b26645df4c2f87ebc0992ab177fba51db92c2a");
        let bob = array("5dab087e624a8a4b79e17f8b83800ee66f3bb1292618b6fd1c2f8b27ff88e0eb");
        let alice_public =
            array("8520f0098930a754748b7ddcb43ef75a0dbf3a0d26381af4eba4a98eaa9b4e6a");
        let bob_public = array("de9edb7d7b7dc1b4d35b61c2ece435373f8343c85b78674dadfc7e146f882b4f");
        let shared = array("4a5d9d5ba4ce2de1728e3bf480350f25e07e21c947d19e3376f09b3c1e161742");
        assert_eq!(x25519_public_key(alice), alice_public);
        assert_eq!(x25519_public_key(bob), bob_public);
        assert_eq!(x25519(alice, bob_public).unwrap(), shared);
        assert_eq!(x25519(bob, alice_public).unwrap(), shared);
        assert!(x25519(alice, [0; 32]).is_err());
        let mut one = [0; 32];
        one[0] = 1;
        assert!(x25519(alice, one).is_err());
    }
    #[test]
    fn field_canonical_edges_and_low_order_points() {
        let mut p = [0xffu8; 32];
        p[0] = 0xed;
        p[31] = 0x7f;
        let zero = Field::decode(p).encode();
        assert_eq!(zero, [0; 32]);
        let mut minus_one = p;
        minus_one[0] -= 1;
        assert_eq!(Field::decode(minus_one).add(Field::ONE).encode(), [0; 32]);
        assert_eq!(Field::ZERO.sub(Field::ONE).encode(), minus_one);
        assert_eq!(
            Field::decode(minus_one).square().encode(),
            Field::ONE.encode()
        );
        let mut eighteen = [0u8; 32];
        eighteen[0] = 18;
        assert_eq!(Field::decode([0xff; 32]).encode(), eighteen);
        let scalar = [0x53; 32];
        assert!(x25519(scalar, p).is_err());
        let mut p_plus_one = p;
        p_plus_one[0] += 1;
        assert!(x25519(scalar, p_plus_one).is_err());
        assert!(x25519(scalar, minus_one).is_err());
    }

    #[test]
    fn rfc7748_iteration_one_thousand() {
        let mut scalar = [0u8; 32];
        scalar[0] = 9;
        let mut coordinate = scalar;
        for _ in 0..1000 {
            let previous = scalar;
            scalar = x25519(scalar, coordinate).unwrap();
            coordinate = previous;
        }
        assert_eq!(
            scalar,
            array("684cf59ba83309552800ef566f2f4d3c1c3887c49360e3875f2eb94d99532c51")
        );
    }

    #[test]
    fn rfc7748_iteration_one_and_noncanonical_input() {
        let mut base = [0u8; 32];
        base[0] = 9;
        assert_eq!(
            x25519(base, base).unwrap(),
            array("422c8e7a6227d7bca1350b3e2bb7279f7897b87bb6854b783c60e80311ae3079")
        );
        // u and u + p represent the same coordinate when both fit in 255 bits.
        let mut noncanonical = [0xff; 32];
        noncanonical[0] = 0xf6;
        noncanonical[31] = 0x7f;
        assert_eq!(
            x25519(base, noncanonical).unwrap(),
            x25519(base, base).unwrap()
        );
        let mut top_bit = base;
        top_bit[31] |= 0x80;
        assert_eq!(x25519(base, top_bit).unwrap(), x25519(base, base).unwrap());
    }
}
