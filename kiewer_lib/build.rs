// The target triple and the profile, for the build line: cargo hands them
// to a build script and to nothing else, so this script passes them on as
// environment variables the crate reads with env!.
//
// And the viewer for the browser: scripts/build-viewer.sh writes
// wasm-bindgen's output to kiewer_viewer/dist (git-ignored), and when
// both files are there this script turns on the `kiewer_viewer_embedded`
// cfg and names them, so web/viewer.rs embeds them with include_bytes!
// and the server carries the viewer in its one binary (the Dockerfile
// copies the binary and nothing else). Without them the server builds
// and the viewer's asset routes answer 503 naming the script: a fresh
// clone runs `cargo run` before it ever builds the viewer, and the
// pre-commit hook's snapshot never has the files. KIEWER_VIEWER_REQUIRED=1
// (scripts/release-build.sh) makes their absence a build error, so a
// release cannot ship without the viewer.
//
// The embedded files are the caching policy's static assets (ADR 0003;
// docs/v1-scope.md, Caching): each is named by a prefix of the SHA-256
// of its bytes, `kiewer_viewer_bg.<hash>.wasm`, so the server can serve
// it as immutable for a year and a new build is a new URL, and each is
// written under OUT_DIR as itself, as brotli (`.br`) and as gzip
// (`.gz`), so the route sends the smallest coding the browser takes
// without compressing at request time. The wasm is hashed first, since
// the script fetches the wasm by name: the script's one mention of the
// plain name becomes the hashed one, and the script is hashed after
// that rewrite. web/viewer.rs reads the names and the six paths from
// the environment this script sets.
use std::env;
use std::fmt::Write as _;
use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

/// How many hex characters of the hash name a file: 16 is 64 bits, far
/// past any chance of two builds colliding, and short enough to read.
const HASH_LEN: usize = 16;

/// The brotli quality (0 to 11): the best, since the files are compressed
/// once per build and served for a year.
const BROTLI_QUALITY: u32 = 11;
/// The brotli window (10 to 24, a power of two of bytes): the largest,
/// which suits one file of a few megabytes decoded by a browser.
const BROTLI_WINDOW: u32 = 24;

fn main() {
    for (from, to) in [("TARGET", "KIEWER_TARGET"), ("PROFILE", "KIEWER_PROFILE")] {
        let value =
            env::var(from).unwrap_or_else(|_| panic!("cargo sets {from} for a build script"));
        println!("cargo::rustc-env={to}={value}");
    }
    println!("cargo::rerun-if-changed=build.rs");

    println!("cargo::rustc-check-cfg=cfg(kiewer_viewer_embedded)");
    println!("cargo::rerun-if-env-changed=KIEWER_VIEWER_REQUIRED");
    let manifest = PathBuf::from(
        env::var_os("CARGO_MANIFEST_DIR")
            .expect("cargo sets CARGO_MANIFEST_DIR for a build script"),
    );
    let viewer = manifest.join("..").join("kiewer_viewer");
    let dist = viewer.join("dist");
    let script = dist.join("kiewer_viewer.js");
    let wasm = dist.join("kiewer_viewer_bg.wasm");
    if script.is_file() && wasm.is_file() {
        println!("cargo::rustc-cfg=kiewer_viewer_embedded");
        println!("cargo::rerun-if-changed={}", script.display());
        println!("cargo::rerun-if-changed={}", wasm.display());
        let out = PathBuf::from(env::var_os("OUT_DIR").expect("cargo sets OUT_DIR"));
        let wasm_bytes = fs::read(&wasm).expect("the wasm is readable");
        let wasm_name = hashed_name("kiewer_viewer_bg", &wasm_bytes, "wasm");
        let script_text = fs::read_to_string(&script).expect("the script is UTF-8");
        let mentions = script_text.matches("kiewer_viewer_bg.wasm").count();
        assert_eq!(
            mentions, 1,
            "the script names the wasm once, where it fetches it; this one names it {mentions} \
             times, so the rewrite to the hashed name needs a look"
        );
        let script_text = script_text.replace("kiewer_viewer_bg.wasm", &wasm_name);
        let script_name = hashed_name("kiewer_viewer", script_text.as_bytes(), "js");
        emit("KIEWER_VIEWER_WASM", &out, &wasm_name, &wasm_bytes);
        emit(
            "KIEWER_VIEWER_JS",
            &out,
            &script_name,
            script_text.as_bytes(),
        );
    } else {
        assert!(
            env::var_os("KIEWER_VIEWER_REQUIRED").is_none_or(|value| value != "1"),
            "KIEWER_VIEWER_REQUIRED=1 but {} has no kiewer_viewer.js and kiewer_viewer_bg.wasm; \
             run scripts/build-viewer.sh first",
            dist.display()
        );
        // The whole viewer crate's directory, so the files' appearance
        // under dist/ reruns this script and the embed switches on.
        println!("cargo::rerun-if-changed={}", viewer.display());
    }
}

/// `<stem>.<hash prefix>.<extension>` for `bytes`.
fn hashed_name(stem: &str, bytes: &[u8], extension: &str) -> String {
    let digest = Sha256::digest(bytes);
    let mut hex = String::with_capacity(digest.len() * 2);
    for byte in &digest {
        write!(hex, "{byte:02x}").expect("writing to a String cannot fail");
    }
    format!("{stem}.{}.{extension}", &hex[..HASH_LEN])
}

/// Write `bytes` under `out` as `name`, `name.br` and `name.gz`, and
/// tell the crate the name and the three paths: `<var>_NAME`, `<var>`,
/// `<var>_BR`, `<var>_GZ`.
fn emit(var: &str, out: &Path, name: &str, bytes: &[u8]) {
    let plain = out.join(name);
    fs::write(&plain, bytes).expect("OUT_DIR is writable");
    let br = out.join(format!("{name}.br"));
    fs::write(&br, brotli(bytes)).expect("OUT_DIR is writable");
    let gz = out.join(format!("{name}.gz"));
    fs::write(&gz, gzip(bytes)).expect("OUT_DIR is writable");
    println!("cargo::rustc-env={var}_NAME={name}");
    println!("cargo::rustc-env={var}={}", plain.display());
    println!("cargo::rustc-env={var}_BR={}", br.display());
    println!("cargo::rustc-env={var}_GZ={}", gz.display());
}

fn brotli(bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(bytes.len() / 4);
    {
        let mut writer =
            brotli::CompressorWriter::new(&mut out, 1 << 16, BROTLI_QUALITY, BROTLI_WINDOW);
        writer
            .write_all(bytes)
            .expect("writing to a Vec cannot fail");
        writer.flush().expect("writing to a Vec cannot fail");
    }
    out
}

fn gzip(bytes: &[u8]) -> Vec<u8> {
    let mut encoder = flate2::write::GzEncoder::new(
        Vec::with_capacity(bytes.len() / 3),
        flate2::Compression::best(),
    );
    encoder
        .write_all(bytes)
        .expect("writing to a Vec cannot fail");
    encoder.finish().expect("writing to a Vec cannot fail")
}
