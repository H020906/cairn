//! Compiles the crate to WebAssembly and checks what came out.
//!
//! The claim this file exists to hold up is in the crate documentation: **no math comes from
//! the host.** That claim is about a compiled artefact, not about source code, and the only way
//! to check it is to compile the artefact and look. A single `f64::sin()` reintroduced anywhere
//! in the crate — or a `mul_add` on a target without a fused multiply-add — would put a call to
//! somebody else's math library back into the module, and the crate documentation's opening
//! table is a measurement of what that costs.
//!
//! `runtime/tests/differential.rs` takes the module built here and runs it through four
//! engines. This file only establishes that the module is the right shape.

// The crate denies these because a panic inside a numeric kernel would corrupt a result rather
// than fail loudly. That reasoning does not carry into a test harness: here a failed `expect`
// means the harness itself is broken, and saying so immediately is correct. The indexing is a
// hand-written parser for a binary format, where the offsets *are* the algorithm.
#![allow(clippy::expect_used, clippy::indexing_slicing)]

use std::path::{Path, PathBuf};
use std::process::Command;

/// Where `rustc` is, given that a test does not inherit a build script's environment.
fn rustc() -> PathBuf {
    if let Ok(path) = std::env::var("RUSTC") {
        return PathBuf::from(path);
    }
    // Cargo sets `CARGO` for tests, and `rustc` lives beside it in every normal installation.
    if let Ok(cargo) = std::env::var("CARGO") {
        let beside =
            Path::new(&cargo).with_file_name(if cfg!(windows) { "rustc.exe" } else { "rustc" });
        if beside.exists() {
            return beside;
        }
    }
    PathBuf::from("rustc")
}

/// Whether the WebAssembly target is installed.
///
/// Absent it, this file has nothing to say, and the tests skip loudly rather than passing
/// quietly. Set `CAIRN_REQUIRE_WASM=1` to turn the skip into a failure, which is what
/// continuous integration should do.
fn wasm_target_available() -> bool {
    let probe = Command::new(rustc())
        .args([
            "--target",
            "wasm32-unknown-unknown",
            "--print",
            "target-libdir",
        ])
        .output();
    match probe {
        Ok(out) if out.status.success() => {
            let dir = String::from_utf8_lossy(&out.stdout).trim().to_owned();
            !dir.is_empty() && Path::new(&dir).join("libcore-").exists()
                || Path::new(&dir).exists()
                    && std::fs::read_dir(&dir).is_ok_and(|mut d| d.next().is_some())
        }
        _ => false,
    }
}

/// Builds the probe module, returning its bytes.
///
/// Two `rustc` invocations and no cargo: the crate has no dependencies, so there is nothing to
/// resolve, and nesting a cargo inside a cargo would contend for the same build lock.
pub fn build() -> Vec<u8> {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let out = PathBuf::from(env!("CARGO_TARGET_TMPDIR"));
    let probe = manifest.join("../math-probe/probe.rs");
    let rlib = out.join("libcairn_math.rlib");
    let wasm = out.join("math-probe.wasm");

    // Four of these are worth more than they look. Built with `opt-level=3` alone the module is
    // **1,580,252 bytes**; adding `lto=fat`, `codegen-units=1`, `panic=abort` and
    // `strip=symbols` brings it to **14,894 bytes** — a hundredfold smaller and no slower, since
    // `opt-level=z` on its own produces a module twice that size. The difference is almost
    // entirely standard-library machinery that only whole-program optimisation can see is
    // unreachable. A workload is downloaded by every volunteer that runs it and committed to by
    // the grid, so this is not a packaging detail.
    //
    // The three link arguments are the ones `docs/WORKLOADS.md` warns authors about: Cairn
    // refuses a module whose memory declares no maximum, no toolchain emits one, and the shadow
    // stack must be shrunk in the same breath or the link fails with `initial memory too small`
    // — a message that never mentions stacks.
    let common = [
        "--edition",
        "2021",
        "--target",
        "wasm32-unknown-unknown",
        "-C",
        "opt-level=3",
        "-C",
        "panic=abort",
        "-C",
        "lto=fat",
        "-C",
        "codegen-units=1",
        "-C",
        "strip=symbols",
        "-C",
        "link-arg=-zstack-size=131072",
        "-C",
        "link-arg=--initial-memory=262144",
        "-C",
        "link-arg=--max-memory=262144",
    ];

    let status = Command::new(rustc())
        .args(common)
        .args(["--crate-type", "rlib", "--crate-name", "cairn_math"])
        .arg("-o")
        .arg(&rlib)
        .arg(manifest.join("src/lib.rs"))
        .status()
        .expect("could not run rustc");
    assert!(
        status.success(),
        "building cairn-math for WebAssembly failed"
    );

    let status = Command::new(rustc())
        .args(common)
        .args(["--crate-type", "cdylib", "--crate-name", "math_probe"])
        .arg("--extern")
        .arg(format!("cairn_math={}", rlib.display()))
        .arg("-o")
        .arg(&wasm)
        .arg(&probe)
        .status()
        .expect("could not run rustc");
    assert!(status.success(), "building the probe workload failed");

    std::fs::read(&wasm).expect("the probe module was not written")
}

/// Reads a LEB128-encoded unsigned integer, advancing `at`.
fn leb(bytes: &[u8], at: &mut usize) -> u32 {
    let (mut value, mut shift) = (0u32, 0);
    loop {
        let byte = bytes[*at];
        *at += 1;
        value |= u32::from(byte & 0x7f) << shift;
        if byte & 0x80 == 0 {
            return value;
        }
        shift += 7;
    }
}

/// Every `(module, name)` the given WebAssembly module imports.
///
/// Hand-rolled rather than delegated to a parser, because a dependency here would be a
/// dependency of the thing whose dependency-freedom is the point.
fn imports(module: &[u8]) -> Vec<(String, String)> {
    assert_eq!(&module[..4], b"\0asm", "not a WebAssembly module");
    let mut at = 8;
    let mut found = Vec::new();
    while at < module.len() {
        let id = module[at];
        at += 1;
        let size = leb(module, &mut at) as usize;
        let end = at + size;
        if id == 2 {
            let count = leb(module, &mut at);
            for _ in 0..count {
                let mut read_name = || {
                    let len = leb(module, &mut at) as usize;
                    let text = String::from_utf8_lossy(&module[at..at + len]).into_owned();
                    at += len;
                    text
                };
                let (from, name) = (read_name(), read_name());
                found.push((from, name));
                // Skip the descriptor, whose shape depends on what kind of thing this is.
                let kind = module[at];
                at += 1;
                match kind {
                    // A function names a type; a global names a value type and a mutability.
                    0 => {
                        leb(module, &mut at);
                    }
                    3 => at += 2,
                    // A table names a reference type first, then falls through to its limits.
                    1 | 2 => {
                        if kind == 1 {
                            at += 1;
                        }
                        let flags = module[at];
                        at += 1;
                        leb(module, &mut at);
                        if flags & 1 == 1 {
                            leb(module, &mut at);
                        }
                    }
                    other => panic!("unknown import kind {other}"),
                }
            }
        }
        at = end;
    }
    found
}

/// The claim in the crate documentation, checked against the compiled module.
#[test]
fn no_host_math_reaches_the_module() {
    if !wasm_target_available() {
        let required = std::env::var("CAIRN_REQUIRE_WASM").is_ok_and(|v| v == "1");
        assert!(
            !required,
            "CAIRN_REQUIRE_WASM=1 but wasm32-unknown-unknown is not installed"
        );
        eprintln!(
            "SKIPPED: wasm32-unknown-unknown is not installed, so the compiled module cannot \
             be inspected. Install it with `rustup target add wasm32-unknown-unknown`; set \
             CAIRN_REQUIRE_WASM=1 to make this a failure instead of a skip."
        );
        return;
    }

    let module = build();
    let found = imports(&module);

    // Exactly Cairn's own host interface, and nothing else. If a transcendental function were
    // taken from the host — the obvious way to write this crate, and the one the opening table
    // of the crate documentation rules out — it would appear here as a third entry.
    let mut names: Vec<_> = found.iter().map(|(m, n)| format!("{m}.{n}")).collect();
    names.sort();
    assert_eq!(
        names,
        vec!["cairn.input".to_owned(), "cairn.output".to_owned()],
        "the module imports something other than Cairn's two host functions"
    );

    // And it is a real module rather than an empty one that trivially imports nothing.
    println!(
        "math-probe.wasm is {} bytes and imports exactly {names:?}",
        module.len()
    );
    assert!(
        module.len() > 4096,
        "the module is too small to contain the math"
    );
    // A ceiling too, because the flags above are load-bearing and silently losing one of them
    // would multiply this by a hundred without breaking anything else.
    assert!(
        module.len() < 64 * 1024,
        "the module got large: check the codegen flags"
    );
}
