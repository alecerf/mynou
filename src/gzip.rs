//! Gzip (RFC 1952) over DEFLATE (RFC 1951), written with the standard
//! library only: LZ77 matches found through hash chains, then Huffman codes
//! built for each block. Mynou only writes gzip, for its log archives and the
//! debug report; `gunzip`, or a double click on a Mac, reads them.

use std::{cmp::Reverse, collections::BinaryHeap};

/// The CRC-32 of gzip (ISO-HDLC): reflected, polynomial 0xEDB88320.
pub fn crc32(data: &[u8]) -> u32 {
    const TABLE: [u32; 256] = {
        let mut table = [0u32; 256];
        let mut i = 0;
        while i < 256 {
            let mut c = i as u32;
            let mut k = 0;
            while k < 8 {
                c = if c & 1 == 1 {
                    0xEDB8_8320 ^ (c >> 1)
                } else {
                    c >> 1
                };
                k += 1;
            }
            table[i] = c;
            i += 1;
        }
        table
    };
    !data.iter().fold(!0u32, |c, &b| {
        TABLE[((c ^ u32::from(b)) & 0xff) as usize] ^ (c >> 8)
    })
}

/// `data` as one complete gzip member. Members written one after the other
/// in a file read back as one text.
pub fn compress(data: &[u8]) -> Vec<u8> {
    // Magic, deflate, no flags, no time, no extra flags, Unix.
    let mut out = vec![0x1f, 0x8b, 8, 0, 0, 0, 0, 0, 0, 3];
    out.extend(deflate(data));
    out.extend(crc32(data).to_le_bytes());
    out.extend((data.len() as u32).to_le_bytes());
    out
}

const WINDOW: usize = 1 << 15;
const MIN_MATCH: usize = 3;
const MAX_MATCH: usize = 258;
const HASH_BITS: u32 = 15;
/// How many earlier places with the same three bytes are tried per match.
const CHAIN: usize = 48;
/// Tokens per block: each block gets its own Huffman codes.
const BLOCK: usize = 1 << 15;
const END: usize = 256;

#[derive(Clone, Copy)]
enum Token {
    Literal(u8),
    Copy { length: u16, distance: u16 },
}

const LENGTH_BASE: [u16; 29] = [
    3, 4, 5, 6, 7, 8, 9, 10, 11, 13, 15, 17, 19, 23, 27, 31, 35, 43, 51, 59, 67, 83, 99, 115, 131,
    163, 195, 227, 258,
];
const LENGTH_EXTRA: [u8; 29] = [
    0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 2, 2, 3, 3, 3, 3, 4, 4, 4, 4, 5, 5, 5, 5, 0,
];
const DISTANCE_BASE: [u16; 30] = [
    1, 2, 3, 4, 5, 7, 9, 13, 17, 25, 33, 49, 65, 97, 129, 193, 257, 385, 513, 769, 1025, 1537,
    2049, 3073, 4097, 6145, 8193, 12289, 16385, 24577,
];
const DISTANCE_EXTRA: [u8; 30] = [
    0, 0, 0, 0, 1, 1, 2, 2, 3, 3, 4, 4, 5, 5, 6, 6, 7, 7, 8, 8, 9, 9, 10, 10, 11, 11, 12, 12, 13,
    13,
];
/// The order in which the code length code lengths are sent.
const CODE_LENGTH_ORDER: [usize; 19] = [
    16, 17, 18, 0, 8, 7, 9, 6, 10, 5, 11, 4, 12, 3, 13, 2, 14, 1, 15,
];

/// The code of `value` in a table of bases: its index and the extra bits.
fn slot(bases: &[u16], value: u16) -> usize {
    bases.iter().rposition(|&base| base <= value).unwrap_or(0)
}

/// Bits written least significant first, as DEFLATE packs them.
struct Bits {
    out: Vec<u8>,
    acc: u64,
    count: u32,
}

impl Bits {
    fn put(&mut self, value: u32, count: u32) {
        self.acc |= u64::from(value) << self.count;
        self.count += count;
        while self.count >= 8 {
            self.out.push(self.acc as u8);
            self.acc >>= 8;
            self.count -= 8;
        }
    }
    fn finish(mut self) -> Vec<u8> {
        if self.count > 0 {
            self.out.push(self.acc as u8);
        }
        self.out
    }
}

/// The raw DEFLATE stream of `data`.
fn deflate(data: &[u8]) -> Vec<u8> {
    let tokens = matches(data);
    let mut bits = Bits {
        out: Vec::with_capacity(data.len() / 3 + 64),
        acc: 0,
        count: 0,
    };
    let mut blocks = tokens.chunks(BLOCK).peekable();
    if blocks.peek().is_none() {
        block(&mut bits, &[], true);
    }
    while let Some(tokens) = blocks.next() {
        block(&mut bits, tokens, blocks.peek().is_none());
    }
    bits.finish()
}

/// LZ77: each place either repeats the longest earlier run found among the
/// last places with the same three bytes, or is a literal byte.
fn matches(data: &[u8]) -> Vec<Token> {
    const NONE: u32 = u32::MAX;
    let hash = |i: usize| {
        let key = u32::from(data[i]) | u32::from(data[i + 1]) << 8 | u32::from(data[i + 2]) << 16;
        (key.wrapping_mul(0x9E37_79B1) >> (32 - HASH_BITS)) as usize
    };
    let mut head = vec![NONE; 1 << HASH_BITS];
    let mut previous = vec![NONE; data.len()];
    let insert = |i: usize, head: &mut [u32], previous: &mut [u32]| {
        if i + MIN_MATCH <= data.len() {
            let h = hash(i);
            previous[i] = head[h];
            head[h] = i as u32;
        }
    };
    let mut tokens = Vec::with_capacity(data.len() / 2);
    let mut i = 0;
    while i < data.len() {
        let mut best = (0, 0);
        if i + MIN_MATCH <= data.len() {
            let limit = (data.len() - i).min(MAX_MATCH);
            let mut candidate = head[hash(i)];
            let mut tries = 0;
            while candidate != NONE && tries < CHAIN {
                let start = candidate as usize;
                if i - start > WINDOW {
                    break;
                }
                // A longer run must at least match one byte past the best so far.
                if data[start + best.0] == data[i + best.0] || best.0 == 0 {
                    let length = data[start..start + limit]
                        .iter()
                        .zip(&data[i..i + limit])
                        .take_while(|(a, b)| a == b)
                        .count();
                    if length > best.0 {
                        best = (length, i - start);
                        if length == limit {
                            break;
                        }
                    }
                }
                candidate = previous[start];
                tries += 1;
            }
        }
        if best.0 >= MIN_MATCH {
            tokens.push(Token::Copy {
                length: best.0 as u16,
                distance: best.1 as u16,
            });
            for j in i..i + best.0 {
                insert(j, &mut head, &mut previous);
            }
            i += best.0;
        } else {
            tokens.push(Token::Literal(data[i]));
            insert(i, &mut head, &mut previous);
            i += 1;
        }
    }
    tokens
}

/// Huffman code lengths for `frequencies`, none longer than `limit`. A rare
/// symbol that would go past it is made more common until the tree fits.
fn code_lengths(frequencies: &[u32], limit: u8) -> Vec<u8> {
    let mut weights = frequencies.to_vec();
    // Every tree holds at least two codes, so each code takes a bit.
    for symbol in 0..weights.len() {
        if weights.iter().filter(|&&w| w > 0).count() >= 2 {
            break;
        }
        if weights[symbol] == 0 {
            weights[symbol] = 1;
        }
    }
    loop {
        let lengths = huffman(&weights);
        if lengths.iter().all(|&l| l <= limit) {
            return lengths;
        }
        for weight in weights.iter_mut().filter(|w| **w > 0) {
            *weight = (*weight >> 1).max(1);
        }
    }
}

/// Plain Huffman code lengths: the depth of each leaf in the merged tree.
fn huffman(weights: &[u32]) -> Vec<u8> {
    let mut parent = vec![usize::MAX; weights.len()];
    let mut heap: BinaryHeap<Reverse<(u64, usize)>> = weights
        .iter()
        .enumerate()
        .filter(|(_, w)| **w > 0)
        .map(|(symbol, w)| Reverse((u64::from(*w), symbol)))
        .collect();
    while heap.len() > 1 {
        let Reverse((a, x)) = heap.pop().unwrap_or(Reverse((0, 0)));
        let Reverse((b, y)) = heap.pop().unwrap_or(Reverse((0, 0)));
        let node = parent.len();
        parent.push(usize::MAX);
        parent[x] = node;
        parent[y] = node;
        heap.push(Reverse((a + b, node)));
    }
    (0..weights.len())
        .map(|symbol| {
            if weights[symbol] == 0 {
                return 0;
            }
            let mut depth = 0u8;
            let mut node = symbol;
            while parent[node] != usize::MAX {
                node = parent[node];
                depth = depth.saturating_add(1);
            }
            depth
        })
        .collect()
}

/// Canonical codes for `lengths`, bit-reversed to be written least
/// significant first.
fn codes(lengths: &[u8]) -> Vec<u32> {
    let mut count = [0u32; 16];
    for &length in lengths {
        count[usize::from(length)] += 1;
    }
    count[0] = 0;
    let mut next = [0u32; 16];
    let mut code = 0;
    for bits in 1..16 {
        code = (code + count[bits - 1]) << 1;
        next[bits] = code;
    }
    lengths
        .iter()
        .map(|&length| {
            if length == 0 {
                return 0;
            }
            let code = next[usize::from(length)];
            next[usize::from(length)] += 1;
            code.reverse_bits() >> (32 - u32::from(length))
        })
        .collect()
}

/// The code lengths of both trees, run-length coded with symbols 16 to 18:
/// `(symbol, extra bits, extra count)`.
fn runs(lengths: &[u8]) -> Vec<(u8, u32, u32)> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < lengths.len() {
        let length = lengths[i];
        let run = lengths[i..].iter().take_while(|&&l| l == length).count();
        if length == 0 && run >= 11 {
            let n = run.min(138);
            out.push((18, (n - 11) as u32, 7));
            i += n;
        } else if length == 0 && run >= 3 {
            out.push((17, (run - 3) as u32, 3));
            i += run;
        } else if length != 0 && run >= 4 {
            // The first one is sent as it is, then repeated three to six times.
            out.push((length, 0, 0));
            let n = (run - 1).min(6);
            out.push((16, (n - 3) as u32, 2));
            i += 1 + n;
        } else {
            out.push((length, 0, 0));
            i += 1;
        }
    }
    out
}

/// One block with its own Huffman codes.
fn block(bits: &mut Bits, tokens: &[Token], last: bool) {
    let mut literal_counts = [0u32; 286];
    let mut distance_counts = [0u32; 30];
    literal_counts[END] = 1;
    for token in tokens {
        match *token {
            Token::Literal(byte) => literal_counts[usize::from(byte)] += 1,
            Token::Copy { length, distance } => {
                literal_counts[257 + slot(&LENGTH_BASE, length)] += 1;
                distance_counts[slot(&DISTANCE_BASE, distance)] += 1;
            }
        }
    }
    let literal_lengths = code_lengths(&literal_counts, 15);
    let distance_lengths = code_lengths(&distance_counts, 15);
    let literals = 257.max(
        literal_lengths
            .iter()
            .rposition(|&l| l > 0)
            .map_or(0, |i| i + 1),
    );
    let distances = 1.max(
        distance_lengths
            .iter()
            .rposition(|&l| l > 0)
            .map_or(0, |i| i + 1),
    );
    let all: Vec<u8> = literal_lengths[..literals]
        .iter()
        .chain(&distance_lengths[..distances])
        .copied()
        .collect();
    let runs = runs(&all);
    let mut run_counts = [0u32; 19];
    for &(symbol, _, _) in &runs {
        run_counts[usize::from(symbol)] += 1;
    }
    let run_lengths = code_lengths(&run_counts, 7);
    let run_codes = codes(&run_lengths);
    let sent = 4.max(
        CODE_LENGTH_ORDER
            .iter()
            .rposition(|&symbol| run_lengths[symbol] > 0)
            .map_or(0, |i| i + 1),
    );

    bits.put(u32::from(last), 1);
    bits.put(2, 2);
    bits.put((literals - 257) as u32, 5);
    bits.put((distances - 1) as u32, 5);
    bits.put((sent - 4) as u32, 4);
    for &symbol in &CODE_LENGTH_ORDER[..sent] {
        bits.put(u32::from(run_lengths[symbol]), 3);
    }
    for &(symbol, extra, count) in &runs {
        let symbol = usize::from(symbol);
        bits.put(run_codes[symbol], u32::from(run_lengths[symbol]));
        if count > 0 {
            bits.put(extra, count);
        }
    }

    let literal_codes = codes(&literal_lengths);
    let distance_codes = codes(&distance_lengths);
    let put = |bits: &mut Bits, symbol: usize| {
        bits.put(literal_codes[symbol], u32::from(literal_lengths[symbol]));
    };
    for token in tokens {
        match *token {
            Token::Literal(byte) => put(bits, usize::from(byte)),
            Token::Copy { length, distance } => {
                let l = slot(&LENGTH_BASE, length);
                put(bits, 257 + l);
                bits.put(
                    u32::from(length - LENGTH_BASE[l]),
                    u32::from(LENGTH_EXTRA[l]),
                );
                let d = slot(&DISTANCE_BASE, distance);
                bits.put(distance_codes[d], u32::from(distance_lengths[d]));
                bits.put(
                    u32::from(distance - DISTANCE_BASE[d]),
                    u32::from(DISTANCE_EXTRA[d]),
                );
            }
        }
    }
    put(bits, END);
}

/// Reads gzip back with the system's own gzip, an independent decoder.
#[cfg(test)]
pub(crate) fn gunzip(data: &[u8]) -> Vec<u8> {
    use std::{
        io::Write,
        process::{Command, Stdio},
    };
    let mut child = {
        let _spawning = crate::store::SPAWNING
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        Command::new("gzip")
            .arg("-dc")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap()
    };
    let mut stdin = child.stdin.take().unwrap();
    let input = data.to_vec();
    let writer = std::thread::spawn(move || stdin.write_all(&input));
    let output = child.wait_with_output().unwrap();
    writer.join().unwrap().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    output.stdout
}

#[cfg(test)]
#[path = "gzip_tests.rs"]
mod tests;
