//! Bounded X.509 verification for TLS server authentication, written in Rust std.
//! RSA/SHA-256/SHA-384, P-256/SHA-256 and P-384/SHA-384 are supported. Unsupported critical extensions,
//! algorithms and certificate forms fail closed; no network trust is inferred.
//! An explicitly trusted anchor is trusted for its public key, independently
//! of its historical self-signature. Unsupported peer-chain signatures fail.

mod der;
mod ecc;
mod rsa;

use crate::Result;
use der::{Reader, bit_string, boolean, positive_integer, sequence};
pub use ecc::{verify_ecdsa_p256_sha256, verify_ecdsa_p384_sha384};
#[cfg(test)]
pub(crate) use rsa::sign_pss_sha256_for_test;
pub use rsa::{verify_pkcs1_sha256, verify_pkcs1_sha384, verify_pss_sha256, verify_pss_sha384};
use std::net::IpAddr;

const RSA: &[u8] = &[0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x01, 0x01];
const RSA_SHA256: &[u8] = &[0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x01, 0x0b];
const RSA_SHA384: &[u8] = &[0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x01, 0x0c];
const RSA_PSS: &[u8] = &[0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x01, 0x0a];
const MGF1: &[u8] = &[0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x01, 0x08];
const SHA256: &[u8] = &[0x60, 0x86, 0x48, 0x01, 0x65, 0x03, 0x04, 0x02, 0x01];
const SHA384: &[u8] = &[0x60, 0x86, 0x48, 0x01, 0x65, 0x03, 0x04, 0x02, 0x02];
const EC: &[u8] = &[0x2a, 0x86, 0x48, 0xce, 0x3d, 0x02, 0x01];
const P256: &[u8] = &[0x2a, 0x86, 0x48, 0xce, 0x3d, 0x03, 0x01, 0x07];
const P384: &[u8] = &[0x2b, 0x81, 0x04, 0x00, 0x22];
const ECDSA_SHA384: &[u8] = &[0x2a, 0x86, 0x48, 0xce, 0x3d, 0x04, 0x03, 0x03];
const ECDSA_SHA256: &[u8] = &[0x2a, 0x86, 0x48, 0xce, 0x3d, 0x04, 0x03, 0x02];
const SERVER_AUTH: &[u8] = &[0x2b, 0x06, 0x01, 0x05, 0x05, 0x07, 0x03, 0x01];
const ANY_EKU: &[u8] = &[0x55, 0x1d, 0x25, 0];
const MAX_CERTIFICATE: usize = 64 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PublicKey {
    Rsa { modulus: Vec<u8>, exponent: Vec<u8> },
    EcdsaP256 { point: Vec<u8> },
    EcdsaP384 { point: Vec<u8> },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SignatureAlgorithm {
    RsaSha256,
    RsaSha384,
    RsaPssSha256,
    RsaPssSha384,
    EcdsaP256Sha256,
    EcdsaP384Sha384,
    Unsupported,
}

#[derive(Debug, Clone)]
pub struct Certificate {
    pub public_key: PublicKey,
    pub not_before: u64,
    pub not_after: u64,
    pub dns_names: Vec<String>,
    pub ip_addresses: Vec<IpAddr>,
    encoded: Vec<u8>,
    signed: Vec<u8>,
    signature: Vec<u8>,
    algorithm: SignatureAlgorithm,
    issuer: Vec<u8>,
    subject: Vec<u8>,
    ca: bool,
    path_length: Option<usize>,
    key_usage: Option<u8>,
    server_auth: bool,
}

/// Parse a certificate with canonical definite DER lengths and bounded fields.
pub fn parse_certificate(bytes: &[u8]) -> Result<Certificate> {
    if bytes.is_empty() || bytes.len() > MAX_CERTIFICATE {
        return Err("Certificate: size exceeds the limit".into());
    }
    let mut certificate = sequence(bytes)?;
    let tbs = certificate.expect(0x30)?;
    let outer_algorithm = certificate.expect(0x30)?;
    let algorithm = signature_algorithm(outer_algorithm.encoded)?;
    let signature = bit_string(certificate.expect(0x03)?.body)?.to_vec();
    certificate.finish()?;
    let mut reader = Reader::new(tbs.body);
    let version = if reader.peek() == Some(0xa0) {
        let version = reader.expect(0xa0)?;
        let mut version = Reader::new(version.body);
        let integer = version.expect(0x02)?;
        version.finish()?;
        match integer.body {
            [0] => return Err("Certificate: noncanonical default version".into()),
            [1] => 1,
            [2] => 2,
            _ => return Err("Certificate: unknown version".into()),
        }
    } else {
        0
    };
    let serial = positive_integer(reader.expect(0x02)?.body)?;
    if serial.len() > 20 || serial.iter().all(|b| *b == 0) {
        return Err("Certificate: invalid serial number".into());
    }
    let inner_algorithm = reader.expect(0x30)?;
    if inner_algorithm.encoded != outer_algorithm.encoded {
        return Err("Certificate: inconsistent signature algorithms".into());
    }
    let issuer = reader.expect(0x30)?.encoded.to_vec();
    let mut validity = Reader::new(reader.expect(0x30)?.body);
    let not_before = certificate_time(validity.read()?)?;
    let not_after = certificate_time(validity.read()?)?;
    validity.finish()?;
    if not_before > not_after {
        return Err("Certificate: reversed validity period".into());
    }
    let subject = reader.expect(0x30)?.encoded.to_vec();
    let public_key = public_key(reader.expect(0x30)?.encoded)?;
    let mut parsed = Certificate {
        public_key,
        not_before,
        not_after,
        dns_names: Vec::new(),
        ip_addresses: Vec::new(),
        encoded: bytes.to_vec(),
        signed: tbs.encoded.to_vec(),
        signature,
        algorithm,
        issuer,
        subject,
        ca: false,
        path_length: None,
        key_usage: None,
        server_auth: true,
    };
    if matches!(reader.peek(), Some(0x81 | 0x82)) {
        return Err("Certificate: unique identifiers are unsupported".into());
    }
    if reader.peek() == Some(0xa3) {
        if version != 2 {
            return Err("Certificate: extensions require version v3".into());
        }
        let extensions = reader.expect(0xa3)?;
        parse_extensions(extensions.body, &mut parsed)?;
    }
    reader.finish()?;
    Ok(parsed)
}

fn signature_algorithm(encoded: &[u8]) -> Result<SignatureAlgorithm> {
    let mut reader = sequence(encoded)?;
    let oid = reader.expect(0x06)?.body;
    let algorithm = if oid == RSA_SHA256 || oid == RSA_SHA384 {
        if reader.peek() == Some(0x05) && !reader.expect(0x05)?.body.is_empty() {
            return Err("RSA: invalid NULL parameter".into());
        }
        if oid == RSA_SHA256 {
            SignatureAlgorithm::RsaSha256
        } else {
            SignatureAlgorithm::RsaSha384
        }
    } else if oid == RSA_PSS {
        pss_parameters(reader.expect(0x30)?.body)?
    } else if oid == ECDSA_SHA256 {
        SignatureAlgorithm::EcdsaP256Sha256
    } else if oid == ECDSA_SHA384 {
        SignatureAlgorithm::EcdsaP384Sha384
    } else {
        // An AlgorithmIdentifier has one optional parameters element. Preserve
        // unsupported identifiers for explicit trust anchors only; verification
        // of any peer-provided chain link still rejects this variant.
        if !reader.empty() {
            reader.read()?;
        }
        SignatureAlgorithm::Unsupported
    };
    reader.finish()?;
    Ok(algorithm)
}

fn digest_algorithm(encoded: &[u8], expected_oid: &[u8]) -> Result<()> {
    let mut reader = sequence(encoded)?;
    if reader.expect(0x06)?.body != expected_oid {
        return Err("PSS: incompatible hash algorithm".into());
    }
    if reader.peek() == Some(0x05) && !reader.expect(0x05)?.body.is_empty() {
        return Err("PSS: invalid hash parameter".into());
    }
    reader.finish()
}

fn pss_parameters(encoded: &[u8]) -> Result<SignatureAlgorithm> {
    let mut reader = Reader::new(encoded);
    let hash = reader.expect(0xa0)?;
    let mut hash_reader = sequence(hash.body)?;
    let oid = hash_reader.expect(0x06)?.body;
    let (length, algorithm) = if oid == SHA256 {
        (32, SignatureAlgorithm::RsaPssSha256)
    } else if oid == SHA384 {
        (48, SignatureAlgorithm::RsaPssSha384)
    } else {
        return Err("PSS: SHA-256 or SHA-384 is required".into());
    };
    digest_algorithm(hash.body, oid)?;
    let mask = reader.expect(0xa1)?;
    let mut mask = sequence(mask.body)?;
    if mask.expect(0x06)?.body != MGF1 {
        return Err("PSS: MGF1 is required".into());
    }
    digest_algorithm(mask.expect(0x30)?.encoded, oid)?;
    mask.finish()?;
    let mut salt = Reader::new(reader.expect(0xa2)?.body);
    if salt.expect(0x02)?.body != [length] {
        return Err("PSS: salt length must equal the hash length".into());
    }
    salt.finish()?;
    // RFC 8017 defaults trailerField to 1. Explicit defaults are not DER.
    reader.finish()?;
    Ok(algorithm)
}

fn public_key(encoded: &[u8]) -> Result<PublicKey> {
    let mut reader = sequence(encoded)?;
    let mut algorithm = Reader::new(reader.expect(0x30)?.body);
    let oid = algorithm.expect(0x06)?.body;
    let key = bit_string(reader.expect(0x03)?.body)?;
    reader.finish()?;
    let result = if oid == RSA {
        if algorithm.peek() == Some(0x05) && !algorithm.expect(0x05)?.body.is_empty() {
            return Err("RSA: invalid key parameter".into());
        }
        let mut key = sequence(key)?;
        let modulus = positive_integer(key.expect(0x02)?.body)?.to_vec();
        let exponent = positive_integer(key.expect(0x02)?.body)?.to_vec();
        key.finish()?;
        if !(256..=1024).contains(&modulus.len())
            || modulus[0] < 0x80
            || modulus.last().unwrap() & 1 == 0
            || exponent.is_empty()
            || exponent.len() > 4
            || exponent[0] == 0
        {
            return Err("Certificate: RSA key size is out of bounds".into());
        }
        let value = exponent
            .iter()
            .fold(0u32, |n, byte| (n << 8) | u32::from(*byte));
        if value < 3 || value & 1 == 0 {
            return Err("Certificate: invalid RSA exponent".into());
        }
        PublicKey::Rsa { modulus, exponent }
    } else if oid == EC {
        let curve = algorithm.expect(0x06)?.body;
        if curve == P256 {
            ecc::validate_public_point(key)?;
            PublicKey::EcdsaP256 {
                point: key.to_vec(),
            }
        } else if curve == P384 {
            ecc::validate_public_point_p384(key)?;
            PublicKey::EcdsaP384 {
                point: key.to_vec(),
            }
        } else {
            return Err("Certificate: only P-256 or P-384 is supported".into());
        }
    } else {
        return Err("Certificate: unsupported key type".into());
    };
    algorithm.finish()?;
    Ok(result)
}

fn parse_extensions(encoded: &[u8], certificate: &mut Certificate) -> Result<()> {
    let mut extensions = sequence(encoded)?;
    let mut seen: Vec<Vec<u8>> = Vec::new();
    while !extensions.empty() {
        if seen.len() >= 64 {
            return Err("Certificate: too many extensions".into());
        }
        let mut extension = Reader::new(extensions.expect(0x30)?.body);
        let oid = extension.expect(0x06)?.body;
        if seen.iter().any(|seen| seen == oid) {
            return Err("Certificate: duplicate extension".into());
        }
        seen.push(oid.to_vec());
        let critical = if extension.peek() == Some(0x01) {
            let value = boolean(extension.expect(0x01)?.body)?;
            if !value {
                return Err("Certificate: noncanonical default critical value".into());
            }
            value
        } else {
            false
        };
        let value = extension.expect(0x04)?.body;
        extension.finish()?;
        if oid == [0x55, 0x1d, 0x13] {
            let mut constraints = sequence(value)?;
            if constraints.peek() == Some(0x01) {
                certificate.ca = boolean(constraints.expect(0x01)?.body)?;
                if !certificate.ca {
                    return Err("Certificate: noncanonical default CA constraint".into());
                }
            }
            if constraints.peek() == Some(0x02) {
                let number = positive_integer(constraints.expect(0x02)?.body)?;
                if !certificate.ca || number.len() > 2 {
                    return Err("Certificate: invalid path constraint".into());
                }
                certificate.path_length = Some(
                    number
                        .iter()
                        .fold(0usize, |n, byte| (n << 8) | usize::from(*byte)),
                );
            }
            constraints.finish()?;
        } else if oid == [0x55, 0x1d, 0x0f] {
            let mut outer = Reader::new(value);
            let usage = outer.expect(0x03)?.body;
            outer.finish()?;
            if usage.len() < 2
                || usage.len() > 3
                || usage[0] > 7
                || usage.last().unwrap() & ((1u8 << usage[0]) - 1) != 0
            {
                return Err("Certificate: invalid key usage".into());
            }
            certificate.key_usage = Some(usage[1]);
        } else if oid == [0x55, 0x1d, 0x11] {
            let mut names = sequence(value)?;
            let mut count = 0;
            while !names.empty() {
                count += 1;
                if count > 256 {
                    return Err("Certificate: too many alternative names".into());
                }
                let name = names.read()?;
                if name.tag == 0x82 {
                    let name = std::str::from_utf8(name.body)
                        .map_err(|_| "Certificate: invalid DNS name")?;
                    certificate.dns_names.push(normalize_dns(name, true)?);
                } else if name.tag == 0x87 {
                    let address = match name.body {
                        [a, b, c, d] => IpAddr::from([*a, *b, *c, *d]),
                        bytes if bytes.len() == 16 => {
                            let mut address = [0; 16];
                            address.copy_from_slice(bytes);
                            IpAddr::from(address)
                        }
                        _ => return Err("Certificate: invalid SAN IP address".into()),
                    };
                    certificate.ip_addresses.push(address);
                }
            }
            if count == 0 {
                return Err("Certificate: empty SAN list".into());
            }
        } else if oid == [0x55, 0x1d, 0x25] {
            let mut usages = sequence(value)?;
            certificate.server_auth = false;
            let mut count = 0;
            while !usages.empty() {
                count += 1;
                if count > 32 {
                    return Err("Certificate: too many extended key usages".into());
                }
                let usage = usages.expect(0x06)?.body;
                certificate.server_auth |= usage == SERVER_AUTH || usage == ANY_EKU;
            }
            if count == 0 {
                return Err("Certificate: empty extended key usage".into());
            }
        } else if oid == [0x55, 0x1d, 0x1e] {
            return Err("Certificate: name constraints are unsupported".into());
        } else if critical {
            return Err("Certificate: unsupported critical extension".into());
        }
    }
    Ok(())
}

fn normalize_dns(name: &str, wildcard: bool) -> Result<String> {
    let name = name.strip_suffix('.').unwrap_or(name);
    if name.is_empty() || name.len() > 253 || !name.is_ascii() {
        return Err("Invalid DNS name".into());
    }
    let labels: Vec<_> = name.split('.').collect();
    for (i, label) in labels.iter().enumerate() {
        if wildcard && i == 0 && *label == "*" && labels.len() >= 3 {
            continue;
        }
        if label.is_empty()
            || label.len() > 63
            || label.starts_with('-')
            || label.ends_with('-')
            || !label
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-')
        {
            return Err("Invalid DNS name or wildcard".into());
        }
    }
    Ok(name.to_ascii_lowercase())
}

fn matches_hostname(certificate: &Certificate, hostname: &str) -> Result<bool> {
    if let Ok(ip) = hostname.parse::<IpAddr>() {
        return Ok(certificate.ip_addresses.contains(&ip));
    }
    let hostname = normalize_dns(hostname, false)?;
    Ok(certificate.dns_names.iter().any(|name| {
        if let Some(suffix) = name.strip_prefix("*.") {
            hostname
                .split_once('.')
                .is_some_and(|(first, rest)| !first.starts_with("xn--") && rest == suffix)
        } else {
            *name == hostname
        }
    }))
}

fn certificate_time(time: der::Element<'_>) -> Result<u64> {
    let (year, rest) = match time.tag {
        0x17 if time.body.len() == 13 => {
            let year = decimal(&time.body[..2])?;
            (
                if year >= 50 { 1900 + year } else { 2000 + year },
                &time.body[2..],
            )
        }
        0x18 if time.body.len() == 15 => (decimal(&time.body[..4])?, &time.body[4..]),
        _ => return Err("Certificate: invalid DER date".into()),
    };
    if rest[10] != b'Z' || !(1970..=9999).contains(&year) {
        return Err("Certificate: date is out of bounds".into());
    }
    let month = decimal(&rest[..2])?;
    let day = decimal(&rest[2..4])?;
    let hour = decimal(&rest[4..6])?;
    let minute = decimal(&rest[6..8])?;
    let second = decimal(&rest[8..10])?;
    let leap = |year: u64| {
        year.is_multiple_of(4) && (!year.is_multiple_of(100) || year.is_multiple_of(400))
    };
    let month_days = [
        31,
        if leap(year) { 29 } else { 28 },
        31,
        30,
        31,
        30,
        31,
        31,
        30,
        31,
        30,
        31,
    ];
    if !(1..=12).contains(&month)
        || day == 0
        || day > month_days[(month - 1) as usize]
        || hour > 23
        || minute > 59
        || second > 59
    {
        return Err("Certificate: invalid calendar date".into());
    }
    let years = (1970..year)
        .map(|y| if leap(y) { 366 } else { 365 })
        .sum::<u64>();
    let months = month_days[..(month - 1) as usize].iter().sum::<u64>();
    Ok((years + months + day - 1) * 86400 + hour * 3600 + minute * 60 + second)
}
fn decimal(bytes: &[u8]) -> Result<u64> {
    if !bytes.iter().all(u8::is_ascii_digit) {
        return Err("Certificate: invalid date digits".into());
    }
    Ok(bytes
        .iter()
        .fold(0, |n, byte| n * 10 + u64::from(*byte - b'0')))
}

fn verify_signature(certificate: &Certificate, issuer: &PublicKey) -> Result<()> {
    match (certificate.algorithm, issuer) {
        (SignatureAlgorithm::Unsupported, _) => {
            Err("Certificate: unsupported signature algorithm".into())
        }
        (SignatureAlgorithm::RsaSha256, PublicKey::Rsa { modulus, exponent }) => {
            verify_pkcs1_sha256(
                modulus,
                exponent,
                &certificate.signed,
                &certificate.signature,
            )
        }
        (SignatureAlgorithm::RsaSha384, PublicKey::Rsa { modulus, exponent }) => {
            verify_pkcs1_sha384(
                modulus,
                exponent,
                &certificate.signed,
                &certificate.signature,
            )
        }
        (SignatureAlgorithm::RsaPssSha384, PublicKey::Rsa { modulus, exponent }) => {
            verify_pss_sha384(
                modulus,
                exponent,
                &certificate.signed,
                &certificate.signature,
            )
        }
        (SignatureAlgorithm::EcdsaP384Sha384, PublicKey::EcdsaP384 { point }) => {
            verify_ecdsa_p384_sha384(point, &certificate.signed, &certificate.signature)
        }
        (SignatureAlgorithm::EcdsaP384Sha384, PublicKey::EcdsaP256 { point }) => {
            ecc::verify_ecdsa_p256_sha384(point, &certificate.signed, &certificate.signature)
        }
        (SignatureAlgorithm::RsaPssSha256, PublicKey::Rsa { modulus, exponent }) => {
            verify_pss_sha256(
                modulus,
                exponent,
                &certificate.signed,
                &certificate.signature,
            )
        }
        (SignatureAlgorithm::EcdsaP256Sha256, PublicKey::EcdsaP256 { point }) => {
            verify_ecdsa_p256_sha256(point, &certificate.signed, &certificate.signature)
        }
        (SignatureAlgorithm::EcdsaP256Sha256, PublicKey::EcdsaP384 { point }) => {
            ecc::verify_ecdsa_p384_sha256(point, &certificate.signed, &certificate.signature)
        }
        _ => Err("Certificate: incompatible key and signature".into()),
    }
}

fn validate_ca(certificate: &Certificate, subordinate_cas: usize, now: u64) -> Result<()> {
    if now < certificate.not_before || now > certificate.not_after {
        return Err("CA certificate: expired or not yet valid".into());
    }
    if !certificate.ca
        || certificate.key_usage.is_some_and(|usage| usage & 0x04 == 0)
        || !certificate.server_auth
    {
        return Err("Certificate: certificate authority is not authorized".into());
    }
    if certificate
        .path_length
        .is_some_and(|length| subordinate_cas > length)
    {
        return Err("Certificate: path length exceeded".into());
    }
    Ok(())
}

/// Validate an ordered TLS certificate chain against explicit PEM trust roots.
/// No certificate is trusted merely because it is supplied by the peer.
pub fn validate_chain(
    chain_der: &[Vec<u8>],
    roots_pem: &[u8],
    hostname: &str,
    now: u64,
) -> Result<PublicKey> {
    if chain_der.is_empty() || chain_der.len() > 16 {
        return Err("TLS: invalid certificate chain length".into());
    }
    let chain: Vec<Certificate> = chain_der
        .iter()
        .map(|bytes| parse_certificate(bytes))
        .collect::<Result<_>>()?;
    let leaf = &chain[0];
    if now < leaf.not_before || now > leaf.not_after {
        return Err("TLS: certificate is expired or not yet valid".into());
    }
    if leaf.ca || !leaf.server_auth || leaf.key_usage.is_some_and(|usage| usage & 0x80 == 0) {
        return Err("TLS: certificate is not authorized for server authentication".into());
    }
    if !matches_hostname(leaf, hostname)? {
        return Err("TLS: certificate does not match the server hostname".into());
    }
    for i in 1..chain.len() {
        let issuer = &chain[i];
        validate_ca(issuer, i - 1, now)?;
        if chain[i - 1].issuer != issuer.subject {
            return Err("TLS: inconsistent certificate chain names".into());
        }
        verify_signature(&chain[i - 1], &issuer.public_key)?;
    }
    let last = chain.last().unwrap();
    let roots = certificates_from_pem(roots_pem)?;
    for root_der in roots {
        let Ok(root) = parse_certificate(&root_der) else {
            continue;
        };
        if root.encoded == last.encoded {
            validate_ca(&root, chain.len().saturating_sub(2), now)?;
            if root.issuer == root.subject && root.algorithm != SignatureAlgorithm::Unsupported {
                verify_signature(&root, &root.public_key)?;
            }
            return Ok(leaf.public_key.clone());
        }
        if last.issuer == root.subject
            && validate_ca(&root, chain.len() - 1, now).is_ok()
            && verify_signature(last, &root.public_key).is_ok()
        {
            return Ok(leaf.public_key.clone());
        }
    }
    Err("TLS: certificate chain has no compatible trusted root".into())
}

/// Decode PEM certificate data only; private keys and unrelated blocks are ignored.
pub fn certificates_from_pem(bytes: &[u8]) -> Result<Vec<Vec<u8>>> {
    if bytes.len() > 4 * 1024 * 1024 {
        return Err("PEM trust roots: file is too large".into());
    }
    let text = std::str::from_utf8(bytes).map_err(|_| "PEM trust roots: invalid text")?;
    const BEGIN: &str = "-----BEGIN CERTIFICATE-----";
    const END: &str = "-----END CERTIFICATE-----";
    let mut rest = text;
    let mut certificates = Vec::new();
    while let Some(begin) = rest.find(BEGIN) {
        rest = &rest[begin + BEGIN.len()..];
        let end = rest.find(END).ok_or("PEM trust roots: incomplete block")?;
        if certificates.len() >= 256 {
            return Err("PEM trust roots: too many certificates".into());
        }
        certificates.push(base64_certificate(&rest[..end])?);
        rest = &rest[end + END.len()..];
    }
    if certificates.is_empty() {
        return Err("PEM trust roots: no certificates".into());
    }
    Ok(certificates)
}

fn base64_certificate(text: &str) -> Result<Vec<u8>> {
    let chars: Vec<u8> = text
        .bytes()
        .filter(|byte| !byte.is_ascii_whitespace())
        .collect();
    if chars.is_empty()
        || !chars.len().is_multiple_of(4)
        || chars.len() > MAX_CERTIFICATE * 4 / 3 + 4
    {
        return Err("PEM: invalid Base64 length".into());
    }
    let value = |byte: u8| -> Result<u32> {
        match byte {
            b'A'..=b'Z' => Ok(u32::from(byte - b'A')),
            b'a'..=b'z' => Ok(u32::from(byte - b'a') + 26),
            b'0'..=b'9' => Ok(u32::from(byte - b'0') + 52),
            b'+' => Ok(62),
            b'/' => Ok(63),
            _ => Err("PEM: invalid Base64 character".into()),
        }
    };
    let mut decoded = Vec::with_capacity(chars.len() / 4 * 3);
    let chunks = chars.len() / 4;
    for (i, group) in chars.as_chunks::<4>().0.iter().enumerate() {
        let a = value(group[0])?;
        let b = value(group[1])?;
        let c = if group[2] == b'=' {
            0
        } else {
            value(group[2])?
        };
        let d = if group[3] == b'=' {
            0
        } else {
            value(group[3])?
        };
        let padding = usize::from(group[3] == b'=') + usize::from(group[2] == b'=');
        if padding != 0 && i + 1 != chunks
            || group[2] == b'=' && group[3] != b'='
            || padding == 2 && b & 15 != 0
            || padding == 1 && c & 3 != 0
        {
            return Err("PEM: noncanonical Base64 padding".into());
        }
        decoded.push(((a << 2) | (b >> 4)) as u8);
        if padding < 2 {
            decoded.push(((b << 4) | (c >> 2)) as u8);
        }
        if padding == 0 {
            decoded.push(((c << 6) | d) as u8);
        }
    }
    Ok(decoded)
}

#[cfg(test)]
pub(crate) mod test_fixture {
    pub const ROOT_PEM: &[u8] = include_bytes!("pki/fixtures/rsa-root.pem");
    pub const LEAF_DER: &[u8] = include_bytes!("pki/fixtures/rsa-leaf.der");
    pub const ROOT_DER: &[u8] = include_bytes!("pki/fixtures/rsa-root.der");
    pub const MODULUS: &[u8] = include_bytes!("pki/fixtures/rsa-modulus.bin");
    pub const PRIVATE_EXPONENT: &[u8] = include_bytes!("pki/fixtures/rsa-private-exponent.bin");
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn dns_wildcards_are_one_label_only() {
        let mut certificate = Certificate {
            public_key: PublicKey::Rsa {
                modulus: vec![],
                exponent: vec![],
            },
            not_before: 0,
            not_after: 0,
            dns_names: vec!["*.example.com".into()],
            ip_addresses: vec![],
            encoded: vec![],
            signed: vec![],
            signature: vec![],
            algorithm: SignatureAlgorithm::RsaSha256,
            issuer: vec![],
            subject: vec![],
            ca: false,
            path_length: None,
            key_usage: None,
            server_auth: true,
        };
        assert!(matches_hostname(&certificate, "VIDEO.Example.Com.").unwrap());
        assert!(!matches_hostname(&certificate, "x.y.example.com").unwrap());
        assert!(!matches_hostname(&certificate, "example.com").unwrap());
        assert!(!matches_hostname(&certificate, "xn--test.example.com").unwrap());
        assert!(normalize_dns("*.com", true).is_err());
        certificate.ip_addresses.push("127.0.0.1".parse().unwrap());
        assert!(matches_hostname(&certificate, "127.0.0.1").unwrap());
        assert!(!matches_hostname(&certificate, "127.0.0.2").unwrap());
    }
    #[test]
    fn date_validation_and_canonical_pem() {
        assert_eq!(
            certificate_time(der::Element {
                tag: 0x17,
                body: b"700101000000Z",
                encoded: &[]
            })
            .unwrap(),
            0
        );
        assert!(
            certificate_time(der::Element {
                tag: 0x17,
                body: b"230229000000Z",
                encoded: &[]
            })
            .is_err()
        );
        assert_eq!(base64_certificate("YWJj").unwrap(), b"abc");
        assert_eq!(base64_certificate("YQ==").unwrap(), b"a");
        assert!(base64_certificate("YR==").is_err());
        assert!(base64_certificate("YQ==AAAA").is_err());
    }
    #[test]
    fn rejects_noncritical_name_constraints() {
        let mut cert = parse_certificate(test_fixture::LEAF_DER).unwrap();
        let extensions = [
            0x30, 0x0b, 0x30, 0x09, 0x06, 0x03, 0x55, 0x1d, 0x1e, 0x04, 0x02, 0x30, 0,
        ];
        assert!(
            parse_extensions(&extensions, &mut cert)
                .unwrap_err()
                .contains("name constraints")
        );
    }
    #[test]
    fn historical_trust_anchor_does_not_enable_weak_peer_signatures() {
        const OLD_ROOT: &[u8] = include_bytes!("pki/fixtures/rsa-root-sha1.pem");
        const OLD_ROOT_DER: &[u8] = include_bytes!("pki/fixtures/rsa-root-sha1.der");
        const WEAK_LEAF: &[u8] = include_bytes!("pki/fixtures/rsa-leaf-sha1.der");
        let root = parse_certificate(OLD_ROOT_DER).unwrap();
        assert_eq!(root.algorithm, SignatureAlgorithm::Unsupported);
        let now = root.not_before + 60;
        let strong = vec![test_fixture::LEAF_DER.to_vec()];
        assert!(validate_chain(&strong, OLD_ROOT, "localhost", now).is_ok());
        let full = vec![test_fixture::LEAF_DER.to_vec(), OLD_ROOT_DER.to_vec()];
        assert!(validate_chain(&full, OLD_ROOT, "localhost", now).is_ok());
        assert!(validate_chain(&[WEAK_LEAF.to_vec()], OLD_ROOT, "localhost", now).is_err());
        // The same historical certificate received from a peer is not an
        // anchor unless those exact bytes are in the explicit trust store.
        assert!(validate_chain(&full, test_fixture::ROOT_PEM, "localhost", now).is_err());
    }
    #[test]
    fn authenticates_independent_p384_and_rsa384_fixtures() {
        const ROOT: &[u8] = include_bytes!("pki/fixtures/ec384-root.pem");
        const LEAF384: &[u8] = include_bytes!("pki/fixtures/ec384-leaf.der");
        const LEAF256: &[u8] = include_bytes!("pki/fixtures/ec256-leaf.der");
        const MESSAGE: &[u8] = include_bytes!("pki/fixtures/signature-message.txt");
        let leaf = parse_certificate(LEAF384).unwrap();
        let now = leaf.not_before + 60;
        assert!(validate_chain(&[LEAF384.to_vec()], ROOT, "localhost", now).is_ok());
        assert!(validate_chain(&[LEAF256.to_vec()], ROOT, "localhost", now).is_ok());
        let PublicKey::EcdsaP384 { point } = leaf.public_key else {
            panic!("P-384 expected")
        };
        let signature = include_bytes!("pki/fixtures/ec384-signature.der");
        verify_ecdsa_p384_sha384(&point, MESSAGE, signature).unwrap();
        assert!(verify_ecdsa_p384_sha384(&point, b"changed", signature).is_err());
        ecc::verify_ecdsa_p384_sha256(
            &point,
            MESSAGE,
            include_bytes!("pki/fixtures/ec384-sha256-signature.der"),
        )
        .unwrap();
        let PublicKey::EcdsaP256 { point } = parse_certificate(LEAF256).unwrap().public_key else {
            panic!("P-256 expected")
        };
        let signature = include_bytes!("pki/fixtures/ec256-signature.der");
        verify_ecdsa_p256_sha256(&point, MESSAGE, signature).unwrap();
        assert!(verify_ecdsa_p256_sha256(&point, b"changed", signature).is_err());
        ecc::verify_ecdsa_p256_sha384(
            &point,
            MESSAGE,
            include_bytes!("pki/fixtures/ec256-sha384-signature.der"),
        )
        .unwrap();
        verify_pkcs1_sha384(
            test_fixture::MODULUS,
            &[1, 0, 1],
            MESSAGE,
            include_bytes!("pki/fixtures/rsa384-pkcs-signature.bin"),
        )
        .unwrap();
        verify_pss_sha384(
            test_fixture::MODULUS,
            &[1, 0, 1],
            MESSAGE,
            include_bytes!("pki/fixtures/rsa384-pss-signature.bin"),
        )
        .unwrap();
        assert!(
            verify_pkcs1_sha384(
                test_fixture::MODULUS,
                &[1, 0, 1],
                b"changed",
                include_bytes!("pki/fixtures/rsa384-pkcs-signature.bin")
            )
            .is_err()
        );
        assert!(
            verify_pss_sha384(
                test_fixture::MODULUS,
                &[1, 0, 1],
                b"changed",
                include_bytes!("pki/fixtures/rsa384-pss-signature.bin")
            )
            .is_err()
        );
    }
    #[test]
    fn certificate_parser_rejects_truncated_and_trailing_der() {
        for prefix in 0..test_fixture::LEAF_DER.len() {
            assert!(parse_certificate(&test_fixture::LEAF_DER[..prefix]).is_err());
        }
        let mut trailing = test_fixture::LEAF_DER.to_vec();
        trailing.push(0);
        assert!(parse_certificate(&trailing).is_err());
    }
    #[test]
    fn authenticates_rsa_chain_hostname_dates_and_signatures() {
        let leaf = parse_certificate(test_fixture::LEAF_DER).unwrap();
        let now = leaf.not_before + 60;
        let chain = vec![test_fixture::LEAF_DER.to_vec()];
        assert!(validate_chain(&chain, test_fixture::ROOT_PEM, "localhost", now).is_ok());
        assert!(validate_chain(&chain, test_fixture::ROOT_PEM, "127.0.0.1", now).is_ok());
        assert!(validate_chain(&chain, test_fixture::ROOT_PEM, "[::1]", now).is_err());
        assert!(validate_chain(&chain, test_fixture::ROOT_PEM, "::1", now).is_ok());
        assert!(validate_chain(&chain, test_fixture::ROOT_PEM, "other.test", now).is_err());
        assert!(
            validate_chain(
                &chain,
                test_fixture::ROOT_PEM,
                "localhost",
                leaf.not_before - 1
            )
            .is_err()
        );
        assert!(
            validate_chain(
                &chain,
                test_fixture::ROOT_PEM,
                "localhost",
                leaf.not_after + 1
            )
            .is_err()
        );
        let mut corrupt = chain.clone();
        *corrupt[0].last_mut().unwrap() ^= 1;
        assert!(validate_chain(&corrupt, test_fixture::ROOT_PEM, "localhost", now).is_err());
        let mut full = chain;
        full.push(test_fixture::ROOT_DER.to_vec());
        assert!(validate_chain(&full, test_fixture::ROOT_PEM, "localhost", now).is_ok());
        let signed = sign_pss_sha256_for_test(
            test_fixture::MODULUS,
            test_fixture::PRIVATE_EXPONENT,
            b"TLS test message",
        )
        .unwrap();
        assert!(
            verify_pss_sha256(
                test_fixture::MODULUS,
                &[1, 0, 1],
                b"TLS test message",
                &signed
            )
            .is_ok()
        );
        assert!(verify_pss_sha256(test_fixture::MODULUS, &[1, 0, 1], b"changed", &signed).is_err());
    }
}
