//! cargo-fuzz target: panic-freedom of the KiCad parsing stack (SPEC §1/§7).
//!
//! Feeds arbitrary bytes through every public entry point of the parser, in both
//! the generic S-expression layer and the KiCad document layer. The ONLY property
//! asserted is the SPEC's panic-freedom contract (SPEC §1): on ANY input the
//! parser returns a `Result` (`Ok` or a typed `Err`) and NEVER panics, overflows
//! the stack, or aborts. libFuzzer turns any panic/abort into a crash artifact.
//!
//! Run (requires the nightly toolchain + cargo-fuzz, which is NOT installed on
//! the stable dev box — see `fuzz/Cargo.toml`):
//!
//! ```text
//! cargo +nightly fuzz run parse_document
//! ```
//!
//! The same panic-freedom property is covered deterministically and on STABLE by
//! `silicon_canvas::sexpr::tests::fuzz_*` and `::parser::tests::fuzz_*`, which is
//! what runs in CI here. This target is the coverage-guided complement.

#![no_main]

use libfuzzer_sys::fuzz_target;
use std::path::Path;

use silicon_canvas::parser;
use silicon_canvas::sexpr;

fuzz_target!(|data: &[u8]| {
    // The parser's public surface is `&str`-typed. Exercise both a lossy-decoded
    // view (so multi-byte / invalid-utf8 boundaries are stressed) and, when the
    // bytes happen to be valid UTF-8, the borrowed str directly.
    let lossy = String::from_utf8_lossy(data);
    drive(&lossy);
    if let Ok(s) = std::str::from_utf8(data) {
        drive(s);
    }
});

/// Push one source string through every parser entry point. None may panic.
fn drive(src: &str) {
    // Generic S-expression layer.
    let _ = sexpr::parse(src);
    let _ = sexpr::parse_many(src);

    // Token stream directly (the lexer must be total on its own).
    let mut lexer = sexpr::Lexer::new(src);
    loop {
        match lexer.next_token() {
            Ok(Some(_)) => continue,
            Ok(None) | Err(_) => break,
        }
    }

    // KiCad document layer, one pass per supported extension so each grammar
    // (schematic / pcb / symbol lib / footprint lib) sees the same bytes.
    for ext in ["kicad_sch", "kicad_pcb", "kicad_sym", "kicad_mod", "txt"] {
        let path = format!("fuzz.{ext}");
        let _ = parser::parse_document(Path::new(&path), src);
    }
}
