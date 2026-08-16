use std::collections::BTreeSet;
use std::fmt::Write as _;

use serde_json::Value;

mod generated {
    include!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/assets/generated/assets.rs"
    ));
}

const GENERATED_ROOT: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/assets/generated/");
const MANIFEST: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/assets/generated/manifest.json"
));
const RUST_TABLE: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/assets/generated/assets.rs"
));

#[test]
fn generated_include_table_matches_manifest_and_checked_bytes() {
    assert_eq!(
        sha256_hex(b"abc"),
        "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
    );
    let manifest: Value = serde_json::from_str(MANIFEST).expect("generated manifest is JSON");
    assert_eq!(manifest["schema_version"], 1);
    assert_eq!(manifest["build_sha256"], generated::BUILD_SHA256);
    assert_eq!(
        manifest["html"]["content"].as_str().unwrap().as_bytes(),
        generated::INDEX_HTML
    );
    assert_eq!(
        sha256_hex(generated::INDEX_HTML),
        generated::INDEX_HTML_SHA256
    );
    assert_eq!(manifest["html"]["sha256"], generated::INDEX_HTML_SHA256);
    assert_eq!(manifest["html"]["mime"], generated::INDEX_HTML_MIME);
    assert_eq!(
        manifest["html"]["cache_control"],
        generated::INDEX_HTML_CACHE_CONTROL
    );

    let files = manifest["files"].as_array().unwrap();
    assert_eq!(files.len(), 6);
    assert_eq!(generated::REPRESENTATIONS.len(), 6);
    let mut combinations = BTreeSet::new();
    for representation in generated::REPRESENTATIONS {
        let entry = files
            .iter()
            .find(|entry| {
                entry["path"]
                    .as_str()
                    .is_some_and(|path| path.ends_with(representation.file_name))
            })
            .expect("every include-table member is declared");
        assert_eq!(entry["url"], representation.url);
        assert_eq!(entry["kind"], representation.kind);
        assert_eq!(entry["encoding"], representation.encoding);
        match representation.content_encoding {
            Some(value) => assert_eq!(entry["content_encoding"], value),
            None => assert!(entry["content_encoding"].is_null()),
        }
        assert_eq!(entry["mime"], representation.mime);
        assert_eq!(entry["cache_control"], representation.cache_control);
        assert_eq!(entry["sha256"], representation.sha256);
        assert_eq!(
            entry["bytes"].as_u64().unwrap(),
            u64::try_from(representation.bytes_len).unwrap()
        );
        assert_eq!(representation.bytes.len(), representation.bytes_len);
        assert_eq!(sha256_hex(representation.bytes), representation.sha256);
        assert!(representation.url.starts_with("./assets/diagnostics-"));
        assert!(representation.url.contains(generated::BUILD_SHA256));
        combinations.insert((representation.kind, representation.encoding));
    }
    assert_eq!(
        combinations,
        BTreeSet::from([
            ("css", "br"),
            ("css", "gzip"),
            ("css", "raw"),
            ("js", "br"),
            ("js", "gzip"),
            ("js", "raw"),
        ])
    );

    assert_eq!(
        sha256_hex(generated::THIRD_PARTY_NOTICES),
        generated::THIRD_PARTY_NOTICES_SHA256
    );
    assert_eq!(
        manifest["notices"]["sha256"],
        generated::THIRD_PARTY_NOTICES_SHA256
    );
    assert_eq!(
        manifest["notices"]["bytes"].as_u64().unwrap(),
        u64::try_from(generated::THIRD_PARTY_NOTICES.len()).unwrap()
    );
    assert!(
        String::from_utf8_lossy(generated::THIRD_PARTY_NOTICES)
            .contains("Troupe Diagnostics Web UI - Third-Party Notices")
    );
}

#[test]
fn generated_source_is_compile_time_only_and_budgeted() {
    assert_eq!(RUST_TABLE.matches("include_bytes!").count(), 7);
    for forbidden in ["std::fs", "Command::new", "flate", "brotli", "node_modules"] {
        assert!(!RUST_TABLE.contains(forbidden));
    }
    assert!(!MANIFEST.contains(".map\""));
    assert!(!MANIFEST.contains("node_modules"));

    let manifest: Value = serde_json::from_str(MANIFEST).unwrap();
    assert!(
        manifest["budgets"]["logical_uncompressed_bytes"]
            .as_u64()
            .unwrap()
            <= 512 * 1024
    );
    assert!(
        manifest["budgets"]["first_load_brotli_bytes"]
            .as_u64()
            .unwrap()
            <= 160 * 1024
    );
    assert!(manifest["budgets"]["all_embedded_bytes"].as_u64().unwrap() <= 768 * 1024);
    assert!(std::path::Path::new(GENERATED_ROOT).is_absolute());
}

fn sha256_hex(input: &[u8]) -> String {
    const INITIAL: [u32; 8] = [
        0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
        0x5be0cd19,
    ];
    const ROUND: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4,
        0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe,
        0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f,
        0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7,
        0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc,
        0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b,
        0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116,
        0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
        0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7,
        0xc67178f2,
    ];

    let bit_length = u64::try_from(input.len()).unwrap().checked_mul(8).unwrap();
    let mut padded = input.to_vec();
    padded.push(0x80);
    while padded.len() % 64 != 56 {
        padded.push(0);
    }
    padded.extend_from_slice(&bit_length.to_be_bytes());

    let mut hash = INITIAL;
    for chunk in padded.chunks_exact(64) {
        let mut schedule = [0_u32; 64];
        for (index, word) in chunk.chunks_exact(4).enumerate() {
            schedule[index] = u32::from_be_bytes(word.try_into().unwrap());
        }
        for index in 16..64 {
            let first = schedule[index - 15].rotate_right(7)
                ^ schedule[index - 15].rotate_right(18)
                ^ (schedule[index - 15] >> 3);
            let second = schedule[index - 2].rotate_right(17)
                ^ schedule[index - 2].rotate_right(19)
                ^ (schedule[index - 2] >> 10);
            schedule[index] = schedule[index - 16]
                .wrapping_add(first)
                .wrapping_add(schedule[index - 7])
                .wrapping_add(second);
        }

        let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut h] = hash;
        for index in 0..64 {
            let choose = (e & f) ^ ((!e) & g);
            let majority = (a & b) ^ (a & c) ^ (b & c);
            let upper_a = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let upper_e = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let first = h
                .wrapping_add(upper_e)
                .wrapping_add(choose)
                .wrapping_add(ROUND[index])
                .wrapping_add(schedule[index]);
            let second = upper_a.wrapping_add(majority);
            h = g;
            g = f;
            f = e;
            e = d.wrapping_add(first);
            d = c;
            c = b;
            b = a;
            a = first.wrapping_add(second);
        }
        for (state, value) in hash.iter_mut().zip([a, b, c, d, e, f, g, h]) {
            *state = state.wrapping_add(value);
        }
    }

    let mut output = String::with_capacity(64);
    for word in hash {
        write!(output, "{word:08x}").unwrap();
    }
    output
}
