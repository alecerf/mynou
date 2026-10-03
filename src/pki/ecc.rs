use super::der::{positive_integer, sequence};
use super::rsa::{Modulus, to_bytes};
use crate::Result;
use crate::crypto::{sha256, sha384};

const PRIME: [u8; 32] = hex(b"ffffffff00000001000000000000000000000000ffffffffffffffffffffffff");
const ORDER: [u8; 32] = hex(b"ffffffff00000000ffffffffffffffffbce6faada7179e84f3b9cac2fc632551");
const CURVE_B: [u8; 32] = hex(b"5ac635d8aa3a93e7b3ebbd55769886bc651d06b0cc53b0f63bce3c3e27d2604b");
const GX: [u8; 32] = hex(b"6b17d1f2e12c4247f8bce6e563a440f277037d812deb33a0f4a13945d898c296");
const GY: [u8; 32] = hex(b"4fe342e2fe1a7f9b8ee7eb4a7c0f9e162bce33576b315ececbb6406837bf51f5");
const fn hex<const N: usize>(bytes: &[u8]) -> [u8; N] {
    let mut out = [0; N];
    let mut i = 0;
    while i < N {
        let a = if bytes[i * 2] <= b'9' {
            bytes[i * 2] - b'0'
        } else {
            bytes[i * 2] - b'a' + 10
        };
        let b = if bytes[i * 2 + 1] <= b'9' {
            bytes[i * 2 + 1] - b'0'
        } else {
            bytes[i * 2 + 1] - b'a' + 10
        };
        out[i] = (a << 4) | b;
        i += 1;
    }
    out
}

const PRIME384: [u8;48] = hex(b"fffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffeffffffff0000000000000000ffffffff");
const ORDER384: [u8;48] = hex(b"ffffffffffffffffffffffffffffffffffffffffffffffffc7634d81f4372ddf581a0db248b0a77aecec196accc52973");
const B384: [u8;48] = hex(b"b3312fa7e23ee7e4988e056be3f82d19181d9c6efe8141120314088f5013875ac656398d8a2ed19d2a85c8edd3ec2aef");
const GX384: [u8;48] = hex(b"aa87ca22be8b05378eb1c71ef320ad746e1d3b628ba79b9859f741e082542a385502f25dbf55296c3a545e3872760ab7");
const GY384: [u8;48] = hex(b"3617de4a96262c6f5d9e98bf9292dc29f8f41dbd289a147ce9da3113b5f0b8c00a60b1ce1d7e819d7a431d7c90ea0e5f");

struct Curve {
    prime: &'static [u8],
    order: &'static [u8],
    b: &'static [u8],
    gx: &'static [u8],
    gy: &'static [u8],
}
const CURVE256: Curve = Curve {
    prime: &PRIME,
    order: &ORDER,
    b: &CURVE_B,
    gx: &GX,
    gy: &GY,
};
const CURVE384: Curve = Curve {
    prime: &PRIME384,
    order: &ORDER384,
    b: &B384,
    gx: &GX384,
    gy: &GY384,
};

struct Point {
    x: Vec<u32>,
    y: Vec<u32>,
    z: Vec<u32>,
}
impl Point {
    fn infinity(field: &Modulus) -> Self {
        Self {
            x: vec![0; field.width()],
            y: field.one(),
            z: vec![0; field.width()],
        }
    }
    fn is_infinity(&self) -> bool {
        self.z.iter().all(|x| *x == 0)
    }
    fn clone_point(&self) -> Self {
        Self {
            x: self.x.clone(),
            y: self.y.clone(),
            z: self.z.clone(),
        }
    }
}

fn scaled(field: &Modulus, value: &[u32], factor: u8) -> Vec<u32> {
    let mut out = vec![0; field.width()];
    for _ in 0..factor {
        out = field.add(&out, value);
    }
    out
}

fn double(field: &Modulus, point: &Point) -> Point {
    if point.is_infinity() || point.y.iter().all(|x| *x == 0) {
        return Point::infinity(field);
    }
    let delta = field.multiply(&point.z, &point.z);
    let gamma = field.multiply(&point.y, &point.y);
    let beta = field.multiply(&point.x, &gamma);
    let alpha = scaled(
        field,
        &field.multiply(
            &field.subtract(&point.x, &delta),
            &field.add(&point.x, &delta),
        ),
        3,
    );
    let x = field.subtract(&field.multiply(&alpha, &alpha), &scaled(field, &beta, 8));
    let y = field.subtract(
        &field.multiply(&alpha, &field.subtract(&scaled(field, &beta, 4), &x)),
        &scaled(field, &field.multiply(&gamma, &gamma), 8),
    );
    let z = scaled(field, &field.multiply(&point.y, &point.z), 2);
    Point { x, y, z }
}

fn add(field: &Modulus, first: &Point, second: &Point) -> Point {
    if first.is_infinity() {
        return second.clone_point();
    }
    if second.is_infinity() {
        return first.clone_point();
    }
    let z1z1 = field.multiply(&first.z, &first.z);
    let z2z2 = field.multiply(&second.z, &second.z);
    let u1 = field.multiply(&first.x, &z2z2);
    let u2 = field.multiply(&second.x, &z1z1);
    let s1 = field.multiply(&first.y, &field.multiply(&second.z, &z2z2));
    let s2 = field.multiply(&second.y, &field.multiply(&first.z, &z1z1));
    if u1 == u2 {
        return if s1 == s2 {
            double(field, first)
        } else {
            Point::infinity(field)
        };
    }
    let h = field.subtract(&u2, &u1);
    let i = field.multiply(&scaled(field, &h, 2), &scaled(field, &h, 2));
    let j = field.multiply(&h, &i);
    let r = scaled(field, &field.subtract(&s2, &s1), 2);
    let v = field.multiply(&u1, &i);
    let x = field.subtract(
        &field.subtract(&field.multiply(&r, &r), &j),
        &scaled(field, &v, 2),
    );
    let y = field.subtract(
        &field.multiply(&r, &field.subtract(&v, &x)),
        &scaled(field, &field.multiply(&s1, &j), 2),
    );
    let z = field.multiply(
        &field.subtract(
            &field.subtract(
                &field.multiply(
                    &field.add(&first.z, &second.z),
                    &field.add(&first.z, &second.z),
                ),
                &z1z1,
            ),
            &z2z2,
        ),
        &h,
    );
    Point { x, y, z }
}

fn scalar_multiply(field: &Modulus, point: &Point, scalar: &[u8]) -> Point {
    let mut result = Point::infinity(field);
    for byte in scalar {
        for bit in (0..8).rev() {
            result = double(field, &result);
            if byte & (1 << bit) != 0 {
                result = add(field, &result, point);
            }
        }
    }
    result
}

fn inverse(field: &Modulus, value: &[u32], modulus: &[u8]) -> Vec<u32> {
    let mut exponent = modulus.to_vec();
    let last = exponent.len() - 1;
    exponent[last] -= 2; // Both supported primes have low byte > 2.
    field.power(value, &exponent)
}

fn public_point(field: &Modulus, bytes: &[u8], curve_b: &[u8]) -> Result<Point> {
    let width = field.width() * 4;
    if bytes.len() != 1 + 2 * width || bytes[0] != 4 {
        return Err("Courbe : point public non compressé attendu".into());
    }
    let x = field.encode(&bytes[1..1 + width])?;
    let y = field.encode(&bytes[1 + width..])?;
    let left = field.multiply(&y, &y);
    let right = field.add(
        &field.subtract(
            &field.multiply(&field.multiply(&x, &x), &x),
            &scaled(field, &x, 3),
        ),
        &field.encode(curve_b)?,
    );
    if left != right {
        return Err("Courbe : point public absent de la courbe".into());
    }
    // P-256 has cofactor one; every non-infinite curve point belongs to its group.
    Ok(Point {
        x,
        y,
        z: field.one(),
    })
}

pub(super) fn validate_public_point(bytes: &[u8]) -> Result<()> {
    public_point(&Modulus::new(&PRIME)?, bytes, &CURVE_B).map(|_| ())
}

pub(super) fn validate_public_point_p384(bytes: &[u8]) -> Result<()> {
    public_point(&Modulus::new(&PRIME384)?, bytes, &B384).map(|_| ())
}

/// Verify canonical DER ECDSA SHA-256 with P-256, on public data only.
pub fn verify_ecdsa_p256_sha256(point: &[u8], message: &[u8], signature: &[u8]) -> Result<()> {
    verify_ecdsa(point, &sha256(message), signature, &CURVE256)
}
/// Verify canonical DER ECDSA SHA-384 with P-384, on public data only.
pub fn verify_ecdsa_p384_sha384(point: &[u8], message: &[u8], signature: &[u8]) -> Result<()> {
    verify_ecdsa(point, &sha384(message), signature, &CURVE384)
}
pub(super) fn verify_ecdsa_p256_sha384(
    point: &[u8],
    message: &[u8],
    signature: &[u8],
) -> Result<()> {
    verify_ecdsa(point, &sha384(message), signature, &CURVE256)
}
pub(super) fn verify_ecdsa_p384_sha256(
    point: &[u8],
    message: &[u8],
    signature: &[u8],
) -> Result<()> {
    verify_ecdsa(point, &sha256(message), signature, &CURVE384)
}

fn reduce_bytes(bytes: &mut [u8], modulus: &[u8]) {
    if &*bytes >= modulus {
        let mut borrow = 0u16;
        for i in (0..bytes.len()).rev() {
            let sub = u16::from(modulus[i]) + borrow;
            let value = u16::from(bytes[i]);
            bytes[i] = value.wrapping_sub(sub) as u8;
            borrow = u16::from(value < sub);
        }
    }
}

fn verify_ecdsa(point: &[u8], message_hash: &[u8], signature: &[u8], curve: &Curve) -> Result<()> {
    let width = curve.prime.len();
    if signature.len() > 2 * width + 8 {
        return Err("ECDSA : signature trop grande".into());
    }
    let mut signature = sequence(signature)?;
    let r = positive_integer(signature.expect(0x02)?.body)?;
    let s = positive_integer(signature.expect(0x02)?.body)?;
    signature.finish()?;
    if r.len() > width || s.len() > width || r.iter().all(|x| *x == 0) || s.iter().all(|x| *x == 0)
    {
        return Err("ECDSA : entier de signature invalide".into());
    }
    let order = Modulus::new(curve.order)?;
    let r = order.encode(r)?;
    let s = order.encode(s)?;
    let inverse_s = inverse(&order, &s, curve.order);
    // ECDSA uses the leftmost order-width hash bits, padded on the left when
    // the hash is shorter than the order (FIPS 186-5).
    let take = width.min(message_hash.len());
    let mut hash = vec![0; width];
    hash[width - take..].copy_from_slice(&message_hash[..take]);
    reduce_bytes(&mut hash, curve.order);
    let u1 = to_bytes(
        &order.decode(&order.multiply(&order.encode(&hash)?, &inverse_s)),
        width,
    );
    let u2 = to_bytes(&order.decode(&order.multiply(&r, &inverse_s)), width);
    let field = Modulus::new(curve.prime)?;
    let public = public_point(&field, point, curve.b)?;
    let generator = Point {
        x: field.encode(curve.gx)?,
        y: field.encode(curve.gy)?,
        z: field.one(),
    };
    let result = add(
        &field,
        &scalar_multiply(&field, &generator, &u1),
        &scalar_multiply(&field, &public, &u2),
    );
    if result.is_infinity() {
        return Err("ECDSA : résultat à l'infini".into());
    }
    let inverse_z = inverse(&field, &result.z, curve.prime);
    let x = field.multiply(&result.x, &field.multiply(&inverse_z, &inverse_z));
    let mut x = to_bytes(&field.decode(&x), width);
    reduce_bytes(&mut x, curve.order);
    if order.encode(&x)? == r {
        Ok(())
    } else {
        Err("ECDSA : signature incorrecte".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn generator() -> Vec<u8> {
        [vec![4], GX.to_vec(), GY.to_vec()].concat()
    }
    #[test]
    fn p384_group_order() {
        let field = Modulus::new(&PRIME384).unwrap();
        let encoded = [vec![4], GX384.to_vec(), GY384.to_vec()].concat();
        let g = public_point(&field, &encoded, &B384).unwrap();
        assert!(scalar_multiply(&field, &g, &ORDER384).is_infinity());
    }
    #[test]
    fn p256_group_identity_and_order() {
        let field = Modulus::new(&PRIME).unwrap();
        let g = public_point(&field, &generator(), &CURVE_B).unwrap();
        assert!(scalar_multiply(&field, &g, &ORDER).is_infinity());
        let first = double(&field, &g);
        let second = scalar_multiply(&field, &g, &[2]);
        let z = inverse(&field, &first.z, &PRIME);
        let x = field.multiply(&first.x, &field.multiply(&z, &z));
        let z = inverse(&field, &second.z, &PRIME);
        let second_x = field.multiply(&second.x, &field.multiply(&z, &z));
        assert_eq!(x, second_x);
        let expected =
            hex::<32>(b"7cf27b188d034f7e8a52380304b51ac3c08969e277f21b35a60b48fc47669978");
        assert_eq!(to_bytes(&field.decode(&x), 32), expected);
        assert!(validate_public_point(&[4; 65]).is_err());
    }
}
