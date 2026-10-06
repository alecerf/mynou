//! Streaming raw DEFLATE, following RFC 1951; no zlib/gzip wrapper is accepted.
use crate::{Result, crypto::Sha256, usenet::yenc::Crc32};
use std::{
    io::{BufRead, BufReader, Read, Take, Write},
    sync::atomic::{AtomicBool, Ordering},
};

const WINDOW: usize = 32_768;
const CHUNK: usize = 65_536;
const LENGTH_BASE: [u16; 29] = [
    3, 4, 5, 6, 7, 8, 9, 10, 11, 13, 15, 17, 19, 23, 27, 31, 35, 43, 51, 59, 67, 83, 99, 115, 131,
    163, 195, 227, 258,
];
const LENGTH_BITS: [u8; 29] = [
    0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 2, 2, 3, 3, 3, 3, 4, 4, 4, 4, 5, 5, 5, 5, 0,
];
const DISTANCE_BASE: [u16; 30] = [
    1, 2, 3, 4, 5, 7, 9, 13, 17, 25, 33, 49, 65, 97, 129, 193, 257, 385, 513, 769, 1025, 1537,
    2049, 3073, 4097, 6145, 8193, 12_289, 16_385, 24_577,
];
const DISTANCE_BITS: [u8; 30] = [
    0, 0, 0, 0, 1, 1, 2, 2, 3, 3, 4, 4, 5, 5, 6, 6, 7, 7, 8, 8, 9, 9, 10, 10, 11, 11, 12, 12, 13,
    13,
];

/// Calculated output checksums. Raw DEFLATE contains no expected checksum.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Decoded {
    pub bytes: u64,
    pub crc32: u32,
    pub sha256: [u8; 32],
}

pub(super) fn active(flag: &AtomicBool) -> Result<()> {
    if flag.load(Ordering::Acquire) {
        Ok(())
    } else {
        Err("Archive operation was cancelled".into())
    }
}

struct Bits<'a, R: Read> {
    reader: BufReader<Take<&'a mut R>>,
    value: u32,
    available: u8,
}
impl<'a, R: Read> Bits<'a, R> {
    fn new(reader: &'a mut R, length: u64) -> Self {
        Self {
            reader: BufReader::with_capacity(CHUNK, reader.take(length)),
            value: 0,
            available: 0,
        }
    }
    fn peek(&mut self, count: u8) -> Result<u32> {
        while self.available < count {
            let bytes = self
                .reader
                .fill_buf()
                .map_err(|e| format!("DEFLATE read: {e}"))?;
            let Some(&byte) = bytes.first() else { break };
            self.value |= u32::from(byte) << self.available;
            self.available += 8;
            self.reader.consume(1);
        }
        Ok(self.value & ((1 << count) - 1))
    }
    fn consume(&mut self, count: u8) -> Result<()> {
        if self.available < count {
            return Err("Truncated DEFLATE bit stream".into());
        }
        self.value >>= count;
        self.available -= count;
        Ok(())
    }
    fn read(&mut self, count: u8) -> Result<u32> {
        let value = self.peek(count)?;
        self.consume(count)?;
        Ok(value)
    }
    fn align(&mut self) -> Result<()> {
        self.consume(self.available % 8)
    }
    fn finish(&mut self) -> Result<()> {
        if self.available >= 8
            || !self
                .reader
                .fill_buf()
                .map_err(|e| format!("DEFLATE read: {e}"))?
                .is_empty()
            || self.reader.get_ref().limit() != 0
        {
            return Err("DEFLATE length mismatch or trailing compressed data".into());
        }
        Ok(())
    }
}

struct Tree {
    lookup: Vec<u32>,
    width: u8,
}
impl Tree {
    fn new(lengths: &[u8], single: bool, empty: bool) -> Result<Self> {
        let mut counts = [0u32; 16];
        for &length in lengths {
            if length > 15 {
                return Err("DEFLATE Huffman length exceeds fifteen bits".into());
            }
            counts[usize::from(length)] += 1;
        }
        let width = lengths.iter().copied().max().unwrap_or(0);
        if width == 0 {
            return if empty {
                Ok(Self {
                    lookup: Vec::new(),
                    width: 0,
                })
            } else {
                Err("Empty DEFLATE Huffman alphabet".into())
            };
        }
        let mut left = 1i32;
        for &count in &counts[1..] {
            left = left * 2 - count as i32;
            if left < 0 {
                return Err("Oversubscribed DEFLATE Huffman alphabet".into());
            }
        }
        if left != 0 && !(single && width == 1 && counts[1] == 1) {
            return Err("Incomplete DEFLATE Huffman alphabet".into());
        }
        let mut next = [0u16; 16];
        let mut code = 0u16;
        for length in 1..=15 {
            code = (code
                + if length == 1 {
                    0
                } else {
                    counts[length - 1] as u16
                })
                << 1;
            next[length] = code;
        }
        let mut lookup = vec![0; 1 << width];
        for (symbol, &length) in lengths.iter().enumerate().filter(|(_, n)| **n != 0) {
            let current = next[usize::from(length)];
            next[usize::from(length)] += 1;
            let reversed = usize::from(current.reverse_bits() >> (16 - length));
            let packed = (u32::from(length) << 16) | symbol as u32;
            for index in (reversed..lookup.len()).step_by(1 << length) {
                lookup[index] = packed;
            }
        }
        Ok(Self { lookup, width })
    }
    fn symbol<R: Read>(&self, bits: &mut Bits<'_, R>) -> Result<u16> {
        if self.width == 0 {
            return Err("DEFLATE used an absent distance alphabet".into());
        }
        let packed = self.lookup[bits.peek(self.width)? as usize];
        if packed == 0 {
            return Err("Invalid DEFLATE Huffman code".into());
        }
        bits.consume((packed >> 16) as u8)?;
        Ok(packed as u16)
    }
}

fn fixed() -> Result<(Tree, Tree)> {
    let mut literals = [8u8; 288];
    literals[144..256].fill(9);
    literals[256..280].fill(7);
    Ok((
        Tree::new(&literals, false, false)?,
        Tree::new(&[5; 32], false, false)?,
    ))
}

fn dynamic<R: Read>(bits: &mut Bits<'_, R>) -> Result<(Tree, Tree)> {
    let literals = bits.read(5)? as usize + 257;
    if literals > 286 {
        return Err("Reserved DEFLATE literal alphabet size".into());
    }
    let distances = bits.read(5)? as usize + 1;
    let code_count = bits.read(4)? as usize + 4;
    let order = [
        16, 17, 18, 0, 8, 7, 9, 6, 10, 5, 11, 4, 12, 3, 13, 2, 14, 1, 15,
    ];
    let mut codes = [0u8; 19];
    for &index in &order[..code_count] {
        codes[index] = bits.read(3)? as u8;
    }
    let code_tree = Tree::new(&codes, false, false)?;
    let count = literals + distances;
    let mut lengths = Vec::with_capacity(count);
    while lengths.len() < count {
        let symbol = code_tree.symbol(bits)?;
        let (value, repeat) = match symbol {
            0..=15 => (symbol as u8, 1),
            16 => (
                *lengths
                    .last()
                    .ok_or("DEFLATE repeat has no previous length")?,
                bits.read(2)? as usize + 3,
            ),
            17 => (0, bits.read(3)? as usize + 3),
            18 => (0, bits.read(7)? as usize + 11),
            _ => return Err("Invalid DEFLATE code-length symbol".into()),
        };
        if lengths.len() + repeat > count {
            return Err("DEFLATE code-length repeat exceeds its alphabet".into());
        }
        lengths.resize(lengths.len() + repeat, value);
    }
    if lengths[256] == 0 {
        return Err("DEFLATE literal alphabet has no end-of-block symbol".into());
    }
    Ok((
        Tree::new(&lengths[..literals], true, false)?,
        Tree::new(&lengths[literals..], true, true)?,
    ))
}

struct Output<'a, W: Write> {
    writer: &'a mut W,
    flag: &'a AtomicBool,
    window: Box<[u8; WINDOW]>,
    pending: Vec<u8>,
    total: u64,
    bound: u64,
    crc: Crc32,
    sha: Sha256,
}
impl<'a, W: Write> Output<'a, W> {
    fn new(writer: &'a mut W, bound: u64, flag: &'a AtomicBool) -> Self {
        Self {
            writer,
            flag,
            window: Box::new([0; WINDOW]),
            pending: Vec::with_capacity(CHUNK),
            total: 0,
            bound,
            crc: Crc32::default(),
            sha: Sha256::new(),
        }
    }
    fn byte(&mut self, byte: u8) -> Result<()> {
        if self.total >= self.bound {
            return Err("DEFLATE output exceeds its captured byte limit".into());
        }
        self.window[self.total as usize & (WINDOW - 1)] = byte;
        self.total += 1;
        self.pending.push(byte);
        if self.pending.len() == CHUNK {
            self.flush()?;
        }
        Ok(())
    }
    fn copy(&mut self, distance: usize, length: usize) -> Result<()> {
        if distance == 0 || distance > WINDOW || distance as u64 > self.total {
            return Err("DEFLATE distance refers before the available window".into());
        }
        if length as u64 > self.bound - self.total {
            return Err("DEFLATE output exceeds its captured byte limit".into());
        }
        for _ in 0..length {
            let byte = self.window[(self.total - distance as u64) as usize & (WINDOW - 1)];
            self.byte(byte)?;
        }
        Ok(())
    }
    fn flush(&mut self) -> Result<()> {
        active(self.flag)?;
        self.writer
            .write_all(&self.pending)
            .map_err(|e| format!("Archive output write: {e}"))?;
        self.crc.update(&self.pending);
        self.sha.update(&self.pending);
        self.pending.clear();
        Ok(())
    }
    fn finish(mut self) -> Result<Decoded> {
        self.flush()?;
        active(self.flag)?;
        Ok(Decoded {
            bytes: self.total,
            crc32: self.crc.finish(),
            sha256: self.sha.finalize(),
        })
    }
}

/// Decode exactly `input_bytes`, with a fixed window and bounded Huffman tables.
/// The caller owns a provisional sink: an error may leave partial bytes there.
/// `active` must stay true; cancellation is checked at blocks, symbols and writes.
pub fn decode<R: Read, W: Write>(
    reader: &mut R,
    input_bytes: u64,
    writer: &mut W,
    max_output_bytes: u64,
    max_blocks: u32,
    flag: &AtomicBool,
) -> Result<Decoded> {
    if input_bytes == 0
        || input_bytes > 1 << 40
        || max_output_bytes > 1 << 40
        || !(1..=65_536).contains(&max_blocks)
    {
        return Err("DEFLATE resource limits exceed the supported bounds".into());
    }
    active(flag)?;
    let mut bits = Bits::new(reader, input_bytes);
    let mut output = Output::new(writer, max_output_bytes, flag);
    let mut symbols = 0u64;
    for _ in 0..max_blocks {
        active(flag)?;
        let last = bits.read(1)? != 0;
        match bits.read(2)? {
            0 => {
                bits.align()?;
                let length = bits.read(16)? as u16;
                if length != !(bits.read(16)? as u16) {
                    return Err("DEFLATE stored block has inconsistent length".into());
                }
                for index in 0..length {
                    if index & 4095 == 0 {
                        active(flag)?;
                    }
                    output.byte(bits.read(8)? as u8)?;
                }
            }
            kind @ (1 | 2) => {
                let (literals, distances) = if kind == 1 {
                    fixed()?
                } else {
                    dynamic(&mut bits)?
                };
                loop {
                    if symbols & 4095 == 0 {
                        active(flag)?;
                    }
                    symbols += 1;
                    match literals.symbol(&mut bits)? {
                        byte @ 0..=255 => output.byte(byte as u8)?,
                        256 => break,
                        symbol @ 257..=285 => {
                            let index = usize::from(symbol - 257);
                            let length = usize::from(LENGTH_BASE[index])
                                + bits.read(LENGTH_BITS[index])? as usize;
                            let distance = usize::from(distances.symbol(&mut bits)?);
                            if distance >= DISTANCE_BASE.len() {
                                return Err("Reserved DEFLATE distance symbol".into());
                            }
                            let distance = usize::from(DISTANCE_BASE[distance])
                                + bits.read(DISTANCE_BITS[distance])? as usize;
                            output.copy(distance, length)?;
                        }
                        _ => return Err("Reserved DEFLATE literal/length symbol".into()),
                    }
                }
            }
            _ => return Err("Reserved DEFLATE block type".into()),
        }
        if last {
            bits.finish()?;
            return output.finish();
        }
    }
    Err("DEFLATE block count exceeds its captured limit".into())
}
