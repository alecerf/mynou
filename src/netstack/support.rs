//! Test helpers shared by the offline stack's unit tests.

use crate::Result;
use crate::json::{self, Value};
use crate::wireguard::Entropy;
use std::io::Read;
use std::net::Ipv4Addr;

pub fn bytes(text: &str) -> Vec<u8> {
    assert!(text.len().is_multiple_of(2));
    (0..text.len())
        .step_by(2)
        .map(|index| u8::from_str_radix(&text[index..index + 2], 16).unwrap())
        .collect()
}

pub fn binary(record: &Value, field: &str) -> Vec<u8> {
    bytes(record.get(field).unwrap().as_str().unwrap())
}

pub fn text<'a>(record: &'a Value, field: &str) -> &'a str {
    record.get(field).unwrap().as_str().unwrap()
}

pub fn address(record: &Value, field: &str) -> Ipv4Addr {
    text(record, field).parse().unwrap()
}

pub fn number(record: &Value, field: &str) -> u64 {
    record.get(field).unwrap().as_u64().unwrap()
}

/// The independent Python std reference vectors that CI generates.
pub fn fixtures() -> Value {
    let path = std::env::var("MYNOU_CRYPTO_FIXTURES")
        .expect("CI must generate independent Python std reference vectors before running tests");
    let file = std::fs::File::open(path).unwrap();
    let mut data = String::new();
    file.take(1024 * 1024 + 1)
        .read_to_string(&mut data)
        .unwrap();
    assert!(data.len() <= 1024 * 1024);
    let value = json::parse(&data).unwrap();
    assert_eq!(value.get("schema").unwrap().as_u64(), Some(1));
    value
}

/// Replays scripted values as the first two bytes of every draw.
pub struct Script {
    values: Vec<u16>,
    calls: usize,
}

impl Script {
    pub fn new(values: &[u16]) -> Self {
        Self {
            values: values.to_vec(),
            calls: 0,
        }
    }

    pub fn calls(&self) -> usize {
        self.calls
    }
}

impl Entropy for Script {
    fn bytes<const N: usize>(&mut self) -> Result<[u8; N]> {
        let value = self.values[self.calls % self.values.len()];
        self.calls += 1;
        let mut out = [0u8; N];
        for (slot, byte) in out.iter_mut().zip(value.to_be_bytes()) {
            *slot = byte;
        }
        Ok(out)
    }
}

pub struct Failing;

impl Entropy for Failing {
    fn bytes<const N: usize>(&mut self) -> Result<[u8; N]> {
        Err("entropy failed".into())
    }
}
