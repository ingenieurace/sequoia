//! Times the column store's encodings: `cargo bench --bench
//! column_encoding`, or with group names (`cargo bench --bench
//! column_encoding -- text`) for only those groups.
//!
//! Each case is 200 chunks of 1,024 values, a row group's worth each, of
//! some kind of data. For each it prints the encoding the store picks (the
//! one that makes the chunk smallest, for most of the chunks), the bytes
//! each value takes, against 8 for an INT, and the fastest of a few runs
//! of encoding every chunk, decoding it, and filtering it on a condition
//! `column = literal` while still encoded, in nanoseconds a value.
//!
//! This times the encodings alone, below the store's pages and SQL: a scan
//! of a column table reads its chunks from pages first.

mod common;

use std::collections::HashMap;
use std::time::Duration;

use sequoia::Value;
use sequoia::predicate::Compare;
use sequoia::storage::column::encoding::{self, Encoding};
use sequoia::types::DataType;

const CHUNK: usize = 1024;
const CHUNKS: usize = 200;

fn main() {
    let ints: Vec<(&str, fn(usize) -> Value)> = vec![
        ("one value", |_| Value::Int(7)),
        ("sorted, runs of 100", |i| Value::Int((i / 100) as i64)),
        ("4 values at random", |i| Value::Int((random(i) % 4) as i64)),
        ("100 values at random", |i| {
            Value::Int((random(i) % 100) as i64)
        }),
        ("counting up", |i| Value::Int(i as i64)),
        ("times, a minute apart", |i| {
            Value::Int(1_700_000_000 + i as i64 * 60 + (random(i) % 10) as i64)
        }),
        ("small, 1% far out", |i| match random(i) % 100 {
            0 => Value::Int(1 << 40),
            r => Value::Int(r as i64),
        }),
        ("any 64-bit value", |i| Value::Int(random(i) as i64)),
        ("1,000 values, 10% NULL", |i| match i % 10 {
            0 => Value::Null,
            _ => Value::Int((random(i) % 1000) as i64),
        }),
    ];
    let texts: Vec<(&str, fn(usize) -> Value)> = vec![
        ("one value", |_| Value::Text("pending".into())),
        ("100 cities", |i| {
            Value::Text(format!("city {:03}", random(i) % 100))
        }),
        ("every one differs", |i| {
            Value::Text(format!("customer {:07}", random(i) % 10_000_000))
        }),
        ("sorted names", |i| Value::Text(format!("customer {i:07}"))),
    ];

    for (group, data_type, cases) in [
        ("int", DataType::Int, ints),
        ("text", DataType::Text, texts),
    ] {
        if !common::wanted(group) {
            continue;
        }
        println!(
            "\n{group:<26} {:<12} {:>12} {:>12} {:>12} {:>12}",
            "encoding", "bytes/value", "encode ns", "decode ns", "filter ns"
        );
        for (name, value) in cases {
            let chunks: Vec<Vec<Value>> = (0..CHUNKS)
                .map(|c| (0..CHUNK).map(|i| value(c * CHUNK + i)).collect())
                .collect();
            let (encode, encoded) = common::fastest(|| {
                chunks
                    .iter()
                    .map(|chunk| encoding::encode(data_type, chunk).unwrap())
                    .collect::<Vec<_>>()
            });
            let (decode, _) = common::fastest(|| {
                for (bytes, chunk) in encoded.iter().zip(&chunks) {
                    let values = encoding::decode(data_type, bytes, CHUNK).unwrap();
                    assert!(values == *chunk, "{name}: a chunk decodes to other values");
                }
            });
            // A value the data has: the first.
            let literal = chunks
                .iter()
                .flatten()
                .find(|v| !v.is_null())
                .unwrap()
                .clone();
            let (filter, _) = common::fastest(|| {
                for bytes in &encoded {
                    std::hint::black_box(
                        encoding::filter(data_type, bytes, CHUNK, Compare::Eq, &literal).unwrap(),
                    );
                }
            });
            let bytes: usize = encoded.iter().map(Vec::len).sum();
            println!(
                "  {name:<24} {:<12} {:>12.2} {:>12.1} {:>12.1} {:>12.1}",
                format!("{:?}", most_used(&encoded)),
                bytes as f64 / (CHUNK * CHUNKS) as f64,
                per_value(encode),
                per_value(decode),
                per_value(filter),
            );
        }
    }
}

/// The encoding most of the chunks use.
fn most_used(chunks: &[Vec<u8>]) -> Encoding {
    let mut counts: HashMap<String, (usize, Encoding)> = HashMap::new();
    for bytes in chunks {
        let encoding = encoding::encoding_of(bytes, CHUNK).unwrap();
        counts
            .entry(format!("{encoding:?}"))
            .or_insert((0, encoding))
            .0 += 1;
    }
    counts
        .into_values()
        .max_by_key(|(count, _)| *count)
        .unwrap()
        .1
}

fn per_value(time: Duration) -> f64 {
    time.as_nanos() as f64 / (CHUNK * CHUNKS) as f64
}

/// A pseudo-random number for position `i`, the same each run (splitmix64).
fn random(i: usize) -> u64 {
    let mut x = (i as u64).wrapping_add(0x9e37_79b9_7f4a_7c15);
    x = (x ^ (x >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    x ^ (x >> 31)
}
