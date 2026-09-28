const WINDOW_SIZE: usize = 8 * 1024;
const MAX_MATCH: usize = 256;
const HASH_BITS: usize = 14;
const HASH_SIZE: usize = 1 << HASH_BITS;

/// The PC archives leave streamed files raw and use LZ2K for these assets.
pub(crate) fn should_compress(path: &str) -> bool {
    let extension = path
        .rsplit('/')
        .next()
        .and_then(|name| name.rsplit_once('.'))
        .map(|(_, extension)| extension);
    extension.is_some_and(|extension| {
        ["an3", "bsa", "dds", "fpk", "ghg", "gsc", "pak", "ter"]
            .iter()
            .any(|known| extension.eq_ignore_ascii_case(known))
    })
}

/// MkDat V3.26 compresses a narrower set of assets than MkDat v4.0.
pub(crate) fn should_compress_legacy(path: &str) -> bool {
    let extension = path
        .rsplit('/')
        .next()
        .and_then(|name| name.rsplit_once('.'))
        .map(|(_, extension)| extension);
    extension.is_some_and(|extension| {
        ["fpk", "ghg", "gsc", "pak"]
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
