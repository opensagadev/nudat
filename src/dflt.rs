//! Nu's DFLT wrapper around raw DEFLATE streams.
//!
//! The game's decoder accepts fixed Huffman blocks (DEFLATE type 1), but its
//! dynamic-tree reader is not compatible with every standards-compliant encoder.
//! Keep each decoded chunk small enough for a single final block, and fall back
//! to a verbatim DFLT chunk whenever the encoder chooses another block type.

use miniz_oxide::deflate::core::{
    compress_to_output, create_comp_flags_from_zip_params, CompressionStrategy, CompressorOxide,
    TDEFLFlush, TDEFLStatus,
};

pub(crate) const BLOCK_SIZE: usize = 16 * 1024;

/// The shipped Android archives compress these asset types. Text, scripts,
/// audio, and other streamed files remain uncompressed even when they would
/// shrink, so they follow the same read paths in the game as the originals.
pub(crate) fn should_compress(path: &str) -> bool {
    let extension = path
        .rsplit('\\')
        .next()
        .and_then(|name| name.rsplit_once('.'))
        .map(|(_, extension)| extension);
    extension.is_some_and(|extension| {
        [
            "android_etc1_tex",
            "bsa",
            "cu2",
            "etc1",
            "fpk",
            "ghg",
            "gsc",
            "ios_pcode",
            "ios_vcode",
            "pak",
            "pvrnc",
            "ter",
            "tex",
        ]
        .iter()
        .any(|known| extension.eq_ignore_ascii_case(known))
    })
}

pub(crate) fn encode_block(input: &[u8]) -> Vec<u8> {
    let flags = create_comp_flags_from_zip_params(6, 0, CompressionStrategy::Fixed as i32);
    let mut compressor = CompressorOxide::new(flags);
    let mut encoded = Vec::new();
    let (status, consumed) =
        compress_to_output(&mut compressor, input, TDEFLFlush::Finish, |part| {
            encoded.extend_from_slice(part);
            true
        });

    // 0b011 is a final, fixed-Huffman block. It needs no Nu-specific tag
    // translation, unlike standard dynamic-Huffman DEFLATE streams.
    if status == TDEFLStatus::Done
        && consumed == input.len()
        && encoded.first().is_some_and(|byte| byte & 0b111 == 0b011)
        && encoded.len() < input.len()
    {
        encoded
    } else {
        input.to_vec()
    }
}
