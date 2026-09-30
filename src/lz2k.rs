use std::io::{self, Error, ErrorKind};

fn invalid(message: &'static str) -> io::Error {
    Error::new(ErrorKind::InvalidData, message)
}

struct Bits<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Bits<'a> {
    #[inline(always)]
    fn get(&mut self, count: usize) -> io::Result<u32> {
        let value = self.peek(count)?;
        self.pos += count;
        Ok(value)
    }

    #[inline(always)]
    fn peek(&self, count: usize) -> io::Result<u32> {
        if count > 24 {
            return Err(invalid("invalid LZ2K bit count"));
        }
        if count == 0 {
            return Ok(0);
        }
        if count > self.data.len().saturating_mul(8).saturating_sub(self.pos) {
            return Err(invalid("truncated LZ2K block"));
        }
        let remaining = &self.data[self.pos / 8..];
        let word = if remaining.len() >= 4 {
            u32::from_be_bytes(remaining[..4].try_into().unwrap())
        } else {
            let mut bytes = [0; 4];
            bytes[..remaining.len()].copy_from_slice(remaining);
            u32::from_be_bytes(bytes)
        };
        Ok((word >> (32 - count - self.pos % 8)) & ((1 << count) - 1))
    }
}

struct Huffman {
    symbols: Vec<u16>,
    first: [u32; 17],
    counts: [u32; 17],
    starts: [usize; 17],
    fast: Vec<u32>,
    table_bits: usize,
    constant: Option<u16>,
}

impl Huffman {
    fn constant(value: u16) -> Self {
        Self {
            symbols: Vec::new(),
            first: [0; 17],
            counts: [0; 17],
            starts: [0; 17],
            fast: Vec::new(),
            table_bits: 0,
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
        let first = next;
        let mut starts = [0; 17];
        let mut total = 0;
        for len in 1..=16 {
            starts[len] = total;
            total += counts[len] as usize;
        }
        let mut symbols = vec![0; total];
        let table_bits = usize::from(*lengths.iter().max().unwrap_or(&0)).min(12);
        let mut fast = vec![0; 1 << table_bits];
        for (symbol, &len) in lengths.iter().enumerate() {
            if len != 0 {
                let c = next[len as usize];
                let index = starts[len as usize] + (c - first[len as usize]) as usize;
                symbols[index] = symbol as u16;
                if len as usize <= table_bits {
                    let start = (c as usize) << (table_bits - len as usize);
                    let end = start + (1 << (table_bits - len as usize));
                    fast[start..end].fill((u32::from(len) << 16) | symbol as u32);
                }
                next[len as usize] += 1;
            }
        }
        Ok(Self {
            symbols,
            first,
            counts,
            starts,
            fast,
            table_bits,
            constant: None,
        })
    }

    #[inline(always)]
    fn decode(&self, bits: &mut Bits<'_>) -> io::Result<u16> {
        if let Some(value) = self.constant {
            return Ok(value);
        }
        // Near the end of a block, a short code may still be valid without
        // a full table prefix. Use the exact-length path there and for long codes.
        let mut minimum = 1;
        if let Ok(prefix) = bits.peek(self.table_bits) {
            let packed = self.fast[prefix as usize];
            if packed != 0 {
                bits.pos += (packed >> 16) as usize;
                return Ok(packed as u16);
            }
            minimum = self.table_bits + 1;
        }
        // Read the remaining prefix once, rather than reloading a word for
        // every individual bit of a long Huffman code.
        let available = (bits.data.len() * 8 - bits.pos).min(16);
        let prefix = bits.peek(available)?;
        for len in minimum..=available {
            let code = prefix >> (available - len);
            let index = code.wrapping_sub(self.first[len]);
            if index < self.counts[len] {
                bits.pos += len;
                return Ok(self.symbols[self.starts[len] + index as usize]);
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

pub(crate) fn decode(data: &[u8], output_len: usize, output: &mut Vec<u8>) -> io::Result<()> {
    let mut bits = Bits { data, pos: 0 };
    output.clear();
    output.reserve(output_len);
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
            let start = output.len() - distance;
            let mut remaining = count;
            while remaining > 0 {
                // Newly copied bytes can supply the next part of an overlapping
                // match; doubling preserves LZ semantics without bytewise pushes.
                let length = remaining.min(output.len() - start);
                output.extend_from_within(start..start + length);
                remaining -= length;
            }
        }
    }
    Ok(())
}

#[cfg(feature = "native")]
#[path = "lz2k/encode.rs"]
mod encode;
#[cfg(feature = "native")]
pub(crate) use encode::{encode_block, should_compress, should_compress_legacy};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bit_windows_match_bitwise_reads_at_every_alignment_and_tail() {
        let data = [0xa7, 0x36, 0xf0, 0x19, 0x82, 0xff, 0x01];
        for pos in 0..=data.len() * 8 {
            for count in 0..=24 {
                let mut bits = Bits { data: &data, pos };
                if pos + count > data.len() * 8 {
                    assert!(bits.get(count).is_err());
                } else {
                    let expected = (pos..pos + count).fold(0u32, |n, at| {
                        (n << 1) | u32::from((data[at / 8] >> (7 - at % 8)) & 1)
                    });
                    assert_eq!(bits.peek(count).unwrap(), expected);
                    assert_eq!(bits.pos, pos);
                    assert_eq!(bits.get(count).unwrap(), expected);
                    assert_eq!(bits.pos, pos + count);
                }
            }
        }
        assert!(Bits {
            data: &data,
            pos: 0
        }
        .get(25)
        .is_err());
    }

    #[test]
    fn huffman_lookup_matches_slow_path_including_long_codes_and_tails() {
        let lengths: Vec<u8> = (1..=16).chain([16]).collect();
        let tree = Huffman::new(&lengths).unwrap();
        for prefix in 0..=u16::MAX {
            let data = prefix.to_be_bytes();
            for pos in [0, 5, 12, 15] {
                let mut fast = Bits { data: &data, pos };
                let mut slow = Bits { data: &data, pos };
                let expected = (|| -> io::Result<u16> {
                    for len in 1..=16 {
                        // This tree has codes 0, 10, 110, ... through sixteen
                        // ones. Keep this reference independent of table layout.
                        if slow.get(1)? == 0 {
                            return Ok(len - 1);
                        }
                    }
                    Ok(16)
                })();
                let actual = tree.decode(&mut fast);
                assert_eq!(actual.as_ref().ok(), expected.as_ref().ok());
                if actual.is_ok() {
                    assert_eq!(fast.pos, slow.pos);
                }
            }
        }
        assert!(Huffman::new(&[1, 1, 1]).is_err());
        assert!(Huffman::new(&[2, 2]).is_err());
        assert!(Huffman::new(&[17]).is_err());
    }

    #[cfg(feature = "native")]
    #[test]
    fn overlapping_matches_and_mixed_literals_roundtrip() {
        for period in [1, 2, 3, 7, 31, 255, 4096, 8192] {
            let mut seed = 123u32;
            let pattern: Vec<u8> = (0..period)
                .map(|_| {
                    seed ^= seed << 13;
                    seed ^= seed >> 17;
                    seed ^= seed << 5;
                    seed as u8
                })
                .collect();
            let input: Vec<u8> = pattern
                .iter()
                .copied()
                .cycle()
                .take(crate::PACK_BLOCK_SIZE)
                .collect();
            let encoded = encode_block(&input);
            assert!(encoded.len() < input.len());
            let mut output = Vec::new();
            decode(&encoded, input.len(), &mut output).unwrap();
            assert_eq!(output, input);
            // Reusing a buffer must reset its length and match history.
            decode(&encoded, input.len(), &mut output).unwrap();
            assert_eq!(output, input);
        }
    }
}
