use std::io::{self, Error, ErrorKind};

fn invalid(message: &'static str) -> io::Error {
    Error::new(ErrorKind::InvalidData, message)
}

struct Bits<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Bits<'a> {
    fn get(&mut self, count: usize) -> io::Result<u32> {
        if count > 24 {
            return Err(invalid("invalid LZ2K bit count"));
        }
        let mut value = 0;
        for _ in 0..count {
            let byte = *self
                .data
                .get(self.pos / 8)
                .ok_or_else(|| invalid("truncated LZ2K block"))?;
            value = (value << 1) | u32::from((byte >> (7 - self.pos % 8)) & 1);
            self.pos += 1;
        }
        Ok(value)
    }

    fn peek(&self, count: usize) -> io::Result<u32> {
        let mut copy = Self {
            data: self.data,
            pos: self.pos,
        };
        copy.get(count)
    }
}

struct Huffman {
    symbols: [Vec<Option<u16>>; 17],
    constant: Option<u16>,
}

impl Huffman {
    fn constant(value: u16) -> Self {
        Self {
            symbols: std::array::from_fn(|_| Vec::new()),
            constant: Some(value),
        }
    }

    fn new(lengths: &[u8]) -> io::Result<Self> {
        let mut counts = [0u32; 17];
        for &len in lengths {
            if len > 16 {
                return Err(invalid("invalid LZ2K Huffman length"));
            }
            counts[len as usize] += 1;
        }
        let mut next = [0u32; 17];
        let mut code = 0;
        for bits in 1..=16 {
            code = (code + if bits == 1 { 0 } else { counts[bits - 1] })
                << if bits == 1 { 0 } else { 1 };
            next[bits] = code;
            if code + counts[bits] > (1u32 << bits) {
                return Err(invalid("oversubscribed LZ2K tree"));
            }
        }
        if code + counts[16] != 1 << 16 {
            return Err(invalid("incomplete LZ2K tree"));
        }
        let mut symbols: [Vec<Option<u16>>; 17] = std::array::from_fn(|_| Vec::new());
        for (symbol, &len) in lengths.iter().enumerate() {
            if len != 0 {
                let c = next[len as usize];
                let row = &mut symbols[len as usize];
                if row.len() <= c as usize {
                    row.resize(c as usize + 1, None);
                }
                row[c as usize] = Some(symbol as u16);
                next[len as usize] += 1;
            }
        }
        Ok(Self {
            symbols,
            constant: None,
        })
    }

    fn decode(&self, bits: &mut Bits<'_>) -> io::Result<u16> {
        if let Some(value) = self.constant {
            return Ok(value);
        }
        let mut code = 0u16;
        for len in 1..=16 {
            code = (code << 1) | bits.get(1)? as u16;
            if let Some(Some(symbol)) = self.symbols[len as usize].get(code as usize) {
                return Ok(*symbol);
            }
        }
        Err(invalid("invalid LZ2K Huffman code"))
    }
}

fn read_offset_lengths(
    bits: &mut Bits<'_>,
    size: usize,
    count_bits: usize,
    zero_after: Option<usize>,
) -> io::Result<Huffman> {
    let n = bits.get(count_bits)? as usize;
    if n == 0 {
        return Ok(Huffman::constant(bits.get(count_bits)? as u16));
    }
    if n > size {
        return Err(invalid("LZ2K offset table is too large"));
    }
    let mut lengths = vec![0; size];
    let mut i = 0;
    while i < n {
        let mut value = bits.peek(3)? as u8;
        if value == 7 {
            let mut offset = 3;
            while bits.peek(offset + 1)? & 1 != 0 {
                value = value
                    .checked_add(1)
                    .ok_or_else(|| invalid("LZ2K code length overflow"))?;
                offset += 1;
                if offset > 16 {
                    return Err(invalid("LZ2K code length overflow"));
                }
            }
            bits.get(offset + 1)?;
        } else {
            bits.get(3)?;
        }
        lengths[i] = value;
        i += 1;
        if zero_after == Some(i) {
            let zeros = bits.get(2)? as usize;
            if i + zeros > size {
                return Err(invalid("LZ2K offset table overflow"));
            }
            i += zeros;
        }
    }
    Huffman::new(&lengths)
}

fn read_literal_lengths(bits: &mut Bits<'_>, code_lengths: &Huffman) -> io::Result<Huffman> {
    let n = bits.get(9)? as usize;
    if n == 0 {
        return Ok(Huffman::constant(bits.get(9)? as u16));
    }
    if n > 510 {
        return Err(invalid("LZ2K literal table is too large"));
    }
    let mut lengths = vec![0; 510];
    let mut i = 0;
    while i < n {
        let value = code_lengths.decode(bits)?;
        if value < 3 {
            let zeros = match value {
                0 => 1,
                1 => bits.get(4)? as usize + 3,
                _ => bits.get(9)? as usize + 20,
            };
            i = i
                .checked_add(zeros)
                .ok_or_else(|| invalid("LZ2K literal table overflow"))?;
            if i > 510 {
                return Err(invalid("LZ2K literal table overflow"));
            }
        } else {
            lengths[i] = (value - 2) as u8;
            i += 1;
        }
    }
    Huffman::new(&lengths)
}

pub(crate) fn decode(data: &[u8], output_len: usize) -> io::Result<Vec<u8>> {
    let mut bits = Bits { data, pos: 0 };
    let mut output = Vec::with_capacity(output_len);
    let mut block_left = 0;
    let mut literals = Huffman::constant(0);
    let mut offsets = Huffman::constant(0);
    while output.len() < output_len {
        if block_left == 0 {
            block_left = bits.get(16)?;
            if block_left == 0 {
                return Err(invalid("empty LZ2K block"));
            }
            let length_codes = read_offset_lengths(&mut bits, 19, 5, Some(3))?;
            literals = read_literal_lengths(&mut bits, &length_codes)?;
            offsets = read_offset_lengths(&mut bits, 14, 4, None)?;
        }
        block_left -= 1;
        let symbol = literals.decode(&mut bits)?;
        if symbol < 256 {
            output.push(symbol as u8);
        } else {
            let offset_code = offsets.decode(&mut bits)? as usize;
            let offset = if offset_code == 0 {
                0
            } else {
                (1usize << (offset_code - 1)) + bits.get(offset_code - 1)? as usize
            };
            let distance = offset + 1;
            let count = symbol as usize - 0xfd;
            if distance > output.len() || count > output_len - output.len() {
                return Err(invalid("invalid LZ2K back-reference"));
            }
            for _ in 0..count {
                let byte = output[output.len() - distance];
                output.push(byte);
            }
        }
    }
    Ok(output)
}

const WINDOW_SIZE: usize = 8 * 1024;
const MAX_MATCH: usize = 256;
const HASH_BITS: usize = 14;
const HASH_SIZE: usize = 1 << HASH_BITS;

/// The PC archives leave streamed files raw and use LZ2K for these assets.
pub(crate) fn should_compress(path: &str) -> bool {
    let extension = path
        .rsplit('\\')
        .next()
        .and_then(|name| name.rsplit_once('.'))
        .map(|(_, extension)| extension);
    extension.is_some_and(|extension| {
        ["an3", "bsa", "dds", "fpk", "ghg", "gsc", "pak", "ter"]
            .iter()
            .any(|known| extension.eq_ignore_ascii_case(known))
    })
}

struct BitWriter {
    bytes: Vec<u8>,
    bits: usize,
}

impl BitWriter {
    fn new(capacity: usize) -> Self {
        Self {
            bytes: Vec::with_capacity(capacity),
            bits: 0,
        }
    }

    fn put(&mut self, value: u32, count: usize) {
        debug_assert!(count <= 16 && (value as u64) < (1u64 << count));
        for shift in (0..count).rev() {
            if self.bits.is_multiple_of(8) {
                self.bytes.push(0);
            }
            let last = self.bytes.last_mut().unwrap();
            *last |= (((value >> shift) & 1) as u8) << (7 - self.bits % 8);
            self.bits += 1;
        }
    }
}

fn hash3(input: &[u8]) -> usize {
    let value = u32::from_le_bytes([input[0], input[1], input[2], 0]);
    (value.wrapping_mul(0x1e35_a7bd) >> (32 - HASH_BITS)) as usize
}

fn insert(input: &[u8], pos: usize, head: &mut [usize], previous: &mut [usize]) {
    if pos + 2 < input.len() {
        let hash = hash3(&input[pos..]);
        previous[pos] = head[hash];
        head[hash] = pos;
    }
}

fn write_literal(writer: &mut BitWriter, symbol: u32) {
    if symbol < 2 {
        writer.put(symbol, 8);
    } else {
        writer.put(symbol + 2, 9);
    }
}

fn write_offset(writer: &mut BitWriter, code: u32) {
    if code < 2 {
        writer.put(code, 3);
    } else {
        writer.put(code + 2, 4);
    }
}

/// Encode one independent LZ2K chunk. The literal and distance trees are
/// complete, as required by the game's Huffman table builder.
/// A chunk that would grow is stored verbatim inside its LZ2K wrapper.
pub(crate) fn encode_block(input: &[u8]) -> Vec<u8> {
    if input.is_empty() {
        return Vec::new();
    }
    debug_assert!(input.len() <= crate::PACK_BLOCK_SIZE);

    let mut writer = BitWriter::new(input.len());
    // A full tree needs two 8-bit and 508 9-bit literal/length codes.
    // The code-length alphabet uses symbols 10 and 11 for those lengths.
    writer.put(0, 16); // token count, filled in after matching
    writer.put(12, 5);
    for _ in 0..3 {
        writer.put(0, 3); // symbols 0..2 unused
    }
    writer.put(3, 2); // skip symbols 3..5
    for _ in 0..4 {
        writer.put(0, 3); // symbols 6..9 unused
    }
    writer.put(1, 3); // symbol 10 has a 1-bit code
    writer.put(1, 3); // symbol 11 has a 1-bit code
    writer.put(510, 9); // all 510 literal/length symbols
    for symbol in 0..510 {
        writer.put(u32::from(symbol >= 2), 1);
    }
    writer.put(14, 4); // two 3-bit and twelve 4-bit distance codes
    for symbol in 0..14 {
        writer.put(if symbol < 2 { 3 } else { 4 }, 3);
    }

    let mut head = vec![usize::MAX; HASH_SIZE];
    let mut previous = vec![usize::MAX; input.len()];
    let mut pos = 0;
    let mut tokens = 0u16;
    while pos < input.len() {
        let mut best_length = 0;
        let mut best_distance = 0;
        if pos + 2 < input.len() {
            let mut candidate = head[hash3(&input[pos..])];
            let limit = (input.len() - pos).min(MAX_MATCH);
            for _ in 0..64 {
                if candidate == usize::MAX || pos - candidate > WINDOW_SIZE {
                    break;
                }
                let mut length = 0;
                while length < limit && input[candidate + length] == input[pos + length] {
                    length += 1;
                }
                if length > best_length {
                    best_length = length;
                    best_distance = pos - candidate;
                    if length == limit {
                        break;
                    }
                }
                candidate = previous[candidate];
            }
        }

        if best_length >= 3 {
            write_literal(&mut writer, (best_length + 0xfd) as u32);
            let offset = best_distance - 1;
            if offset == 0 {
                write_offset(&mut writer, 0);
            } else {
                let code = usize::BITS as usize - offset.leading_zeros() as usize;
                write_offset(&mut writer, code as u32);
                writer.put((offset - (1 << (code - 1))) as u32, code - 1);
            }
            for at in pos..pos + best_length {
                insert(input, at, &mut head, &mut previous);
            }
            pos += best_length;
        } else {
            write_literal(&mut writer, u32::from(input[pos]));
            insert(input, pos, &mut head, &mut previous);
            pos += 1;
        }
        tokens += 1;
    }
    writer.bytes[..2].copy_from_slice(&tokens.to_be_bytes());
    if writer.bytes.len() < input.len() {
        writer.bytes
    } else {
        input.to_vec()
    }
}
