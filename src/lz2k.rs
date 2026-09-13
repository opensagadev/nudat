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
        if symbols.iter().all(Vec::is_empty) {
            return Err(invalid("empty LZ2K Huffman tree"));
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
