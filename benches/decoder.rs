//! Decode-only synthetic workloads; excludes encoding and filesystem access.
#![allow(dead_code, unused_imports)]
#[path = "../src/lz2k.rs"]
mod lz2k;
const PACK_BLOCK_SIZE: usize = 16384;
use std::{
    hint::black_box,
    time::{Duration, Instant},
};

fn main() {
    let mut seed = 42u32;
    let noise: Vec<u8> = (0..4096)
        .map(|_| {
            seed ^= seed << 13;
            seed ^= seed >> 17;
            seed ^= seed << 5;
            seed as u8
        })
        .collect();
    let cases = [
        ("repeated byte", vec![b'A'; PACK_BLOCK_SIZE]),
        (
            "text",
            b"portable archive data with recurring words and identifiers\n".repeat(280),
        ),
        ("mixed literals/matches", noise.repeat(4)),
    ];
    for (name, input) in cases {
        let encoded = lz2k::encode_block(&input);
        assert!(encoded.len() < input.len());
        let mut output = Vec::new();
        lz2k::decode(&encoded, input.len(), &mut output).unwrap();
        assert_eq!(output, input);
        let start = Instant::now();
        let mut iterations = 0;
        while start.elapsed() < Duration::from_secs(1) {
            lz2k::decode(black_box(&encoded), input.len(), &mut output).unwrap();
            black_box(&output);
            iterations += 1;
        }
        let mib = iterations as f64 * input.len() as f64 / 1048576.0;
        println!(
            "{name}: {:.1} MiB/s ({iterations} blocks)",
            mib / start.elapsed().as_secs_f64()
        );
    }
}
