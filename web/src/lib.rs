//! Browser worker adapter. The host supplies synchronous range reads of a local File.
use nudat::{ArchiveIndex, Compression, DecodeChunk};
use std::io::{self, Read, Seek, SeekFrom, Write};
use wasm_bindgen::prelude::*;

#[wasm_bindgen]
extern "C" {
    #[wasm_bindgen(catch, js_namespace = globalThis, js_name = nudatRead)]
    fn read_range(offset: f64, length: u32) -> Result<Vec<u8>, JsValue>;
    #[wasm_bindgen(js_namespace = globalThis, js_name = nudatDecoded)]
    fn report_decoded(bytes: f64);
}

struct DecodedBuffer(Vec<u8>);

impl Write for DecodedBuffer {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let previous = self.0.len();
        self.0.extend_from_slice(bytes);
        // Avoid crossing into JS for every small compression block. Still
        // report the first block so short files get incremental progress.
        if previous == 0 || previous / 262144 != self.0.len() / 262144 {
            report_decoded(self.0.len() as f64);
        }
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

struct Source {
    size: u64,
    position: u64,
}

impl Read for Source {
    fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
        let length = (output.len() as u64).min(self.size.saturating_sub(self.position));
        if length == 0 {
            return Ok(0);
        }
        let bytes = read_range(self.position as f64, length.min(65536) as u32)
            .map_err(|e| io::Error::other(format!("Browser file read failed: {e:?}")))?;
        if bytes.is_empty() || bytes.len() > length as usize {
            return Err(io::Error::other("Invalid browser read length"));
        }
        output[..bytes.len()].copy_from_slice(&bytes);
        self.position += bytes.len() as u64;
        Ok(bytes.len())
    }
}

impl Seek for Source {
    fn seek(&mut self, from: SeekFrom) -> io::Result<u64> {
        let position = match from {
            SeekFrom::Start(n) => i128::from(n),
            SeekFrom::End(n) => i128::from(self.size) + i128::from(n),
            SeekFrom::Current(n) => i128::from(self.position) + i128::from(n),
        };
        if !(0..=i128::from(self.size)).contains(&position) {
            return Err(io::Error::other("Seek outside archive"));
        }
        self.position = position as u64;
        Ok(self.position)
    }
}

#[wasm_bindgen]
pub struct BrowserArchive {
    index: ArchiveIndex,
    source: Source,
}

#[wasm_bindgen]
impl BrowserArchive {
    pub fn chunks(&mut self, path: &str) -> Result<String, JsValue> {
        let chunks = self
            .index
            .chunks(&mut self.source, path, 4 * 1024 * 1024)
            .map_err(|e| JsValue::from_str(&e.to_string()))?;
        Ok(serde_json::Value::Array(chunks.iter().map(|c| serde_json::json!({
            "offset": c.offset, "storedSize": c.stored_size, "size": c.size,
            "outputOffset": c.output_offset, "compression": match c.compression {
                Compression::None => 0, Compression::Lz2k => 2, Compression::Deflate => 3,
            }
        })).collect()).to_string())
    }
    #[wasm_bindgen(constructor)]
    pub fn new(size: f64) -> Result<BrowserArchive, JsValue> {
        if !size.is_finite() || size < 0.0 || size.fract() != 0.0 || size > 9007199254740991.0 {
            return Err(JsValue::from_str("Invalid archive size"));
        }
        let mut source = Source {
            size: size as u64,
            position: 0,
        };
        let index = ArchiveIndex::from_reader(&mut source)
            .map_err(|e| JsValue::from_str(&e.to_string()))?;
        Ok(Self { index, source })
    }

    pub fn metadata(&self) -> String {
        serde_json::json!({
            "format": format!("{:?}", self.index.format()),
            "version": self.index.version(),
            "entries": self.index.entries().iter().map(|e| serde_json::json!({
                "path": e.path, "size": e.size, "storedSize": e.stored_size, "offset": e.offset,
                "compression": format!("{:?}", e.compression)
            })).collect::<Vec<_>>()
        })
        .to_string()
    }

    pub fn extract(&mut self, path: &str) -> Result<Vec<u8>, JsValue> {
        let entry = self
            .index
            .entry(path)
            .ok_or_else(|| JsValue::from_str("File not found"))?;
        if entry.size > 256 * 1024 * 1024 {
            return Err(JsValue::from_str(
                "This operation supports entries up to 256 MiB. Use chunked extraction for this file.",
            ));
        }
        let mut output = DecodedBuffer(Vec::with_capacity(entry.size as usize));
        self.index
            .copy_to(&mut self.source, path, &mut output)
            .map_err(|e| JsValue::from_str(&e.to_string()))?;
        report_decoded(output.0.len() as f64);
        Ok(output.0)
    }
}

/// Decode a planned batch without rebuilding an archive index in each worker.
#[wasm_bindgen]
pub fn decode_chunk(
    file_size: f64,
    offset: f64,
    stored_size: u32,
    size: u32,
    compression: u8,
) -> Result<Vec<u8>, JsValue> {
    if !file_size.is_finite()
        || !offset.is_finite()
        || file_size < 0.0
        || offset < 0.0
        || file_size.fract() != 0.0
        || offset.fract() != 0.0
        || file_size > 9007199254740991.0
        || offset + f64::from(stored_size) > file_size
        || size > 4 * 1024 * 1024
        || stored_size > 4 * 1024 * 1024 + 12
    {
        return Err(JsValue::from_str("Invalid decode chunk"));
    }
    let compression = match compression {
        0 => Compression::None,
        2 => Compression::Lz2k,
        3 => Compression::Deflate,
        _ => return Err(JsValue::from_str("Invalid compression")),
    };
    let chunk = DecodeChunk {
        offset: 0,
        stored_size,
        size,
        output_offset: 0,
        compression,
    };
    // A job already has a bounded contiguous input range. Import it once instead
    // of calling JS and allocating a bridge buffer for every header and block.
    let input = read_range(offset, stored_size)?;
    if input.len() != stored_size as usize {
        return Err(JsValue::from_str("Truncated decode chunk"));
    }
    let mut source = io::Cursor::new(input);
    let mut output = DecodedBuffer(Vec::with_capacity(size as usize));
    chunk
        .copy_to(&mut source, &mut output)
        .map_err(|e| JsValue::from_str(&e.to_string()))?;
    report_decoded(output.0.len() as f64);
    Ok(output.0)
}
