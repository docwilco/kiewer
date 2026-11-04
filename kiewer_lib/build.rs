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
// KIEWER_VIEWER_DIST names another directory to embed from in place of
// kiewer_viewer/dist, and then the files must be there: the browser stage
// of scripts/checks.sh points it at the debug build the wasm stage writes
// under the target directory, so a check gets a server with the viewer in
// it and kiewer_viewer/dist stays what a release embeds.
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
// that rewrite. web/viewer.rs reads the names and the paths from the
// environment this script sets.
//
// The page script, `assets/kiewer_viewer_page.js` (the gestures and the
// beacon the board and schematic pages share), is in the repo and so is
// emitted on every build the same way, viewer or not. It imports the
// viewer's script as `./kiewer_viewer.js`; with the viewer embedded that
// becomes the hashed name before the page script is hashed, so a new
// viewer is a new page script too. Without it the import stays plain
// and fails at the 503, which the page shows in its error box and sends
// as a failed beacon (#96).
//
// The Newstroke glyph table, `kicad_core/src/font/newstroke.txt` (#41),
// is in the repo too and emitted on every build the same way, as
// `kiewer_viewer_newstroke.<hash>.txt`: kicad_core includes it in a
// native build and leaves it out of the wasm, and the page script
// fetches it for the viewer. The page script names it once, as
// `./kiewer_viewer_newstroke.txt`, which becomes the hashed name before
// the page script is hashed, so a new table is a new page script too.
use std::env;
use std::fmt::Write as _;
use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

/// How the page script names the viewer's script in its one import, the
/// text the rewrite to the hashed name replaces.
const PAGE_SCRIPT_IMPORT: &str = "\"./kiewer_viewer.js\"";

/// How the page script names the glyph table in its one fetch, the text
/// the rewrite to the hashed name replaces.
const PAGE_SCRIPT_NEWSTROKE: &str = "\"./kiewer_viewer_newstroke.txt\"";

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
    println!("cargo::rerun-if-env-changed=KIEWER_VIEWER_DIST");
    let manifest = PathBuf::from(
        env::var_os("CARGO_MANIFEST_DIR")
            .expect("cargo sets CARGO_MANIFEST_DIR for a build script"),
    );
    let viewer = manifest.join("..").join("kiewer_viewer");
    let named_dist = env::var_os("KIEWER_VIEWER_DIST")
        .filter(|dir| !dir.is_empty())
        .map(PathBuf::from);
    let dist = named_dist.clone().unwrap_or_else(|| viewer.join("dist"));
    let script = dist.join("kiewer_viewer.js");
    let wasm = dist.join("kiewer_viewer_bg.wasm");
    let out = PathBuf::from(env::var_os("OUT_DIR").expect("cargo sets OUT_DIR"));
    let page_script = manifest.join("assets").join("kiewer_viewer_page.js");
    println!("cargo::rerun-if-changed={}", page_script.display());
    let page_text = fs::read_to_string(&page_script).expect("the page script is UTF-8");
    named_once(
        &page_text,
        PAGE_SCRIPT_IMPORT,
        "the page script imports the viewer's script",
    );
    let page_text = if script.is_file() && wasm.is_file() {
        println!("cargo::rustc-cfg=kiewer_viewer_embedded");
        println!("cargo::rerun-if-changed={}", script.display());
        println!("cargo::rerun-if-changed={}", wasm.display());
        let wasm_bytes = fs::read(&wasm).expect("the wasm is readable");
        let wasm_name = hashed_name("kiewer_viewer_bg", &wasm_bytes, "wasm");
        let script_text = fs::read_to_string(&script).expect("the script is UTF-8");
        named_once(
            &script_text,
            "kiewer_viewer_bg.wasm",
            "the script fetches the wasm",
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
        page_text.replace(PAGE_SCRIPT_IMPORT, &format!("\"./{script_name}\""))
    } else {
        let shown = dist.display();
        assert!(
            named_dist.is_none(),
            "KIEWER_VIEWER_DIST names {shown}, which has no kiewer_viewer.js and \
             kiewer_viewer_bg.wasm; run scripts/build-viewer.sh debug {shown} first"
        );
        assert!(
            env::var_os("KIEWER_VIEWER_REQUIRED").is_none_or(|value| value != "1"),
            "KIEWER_VIEWER_REQUIRED=1 but {} has no kiewer_viewer.js and kiewer_viewer_bg.wasm; \
             run scripts/build-viewer.sh first",
            dist.display()
        );
        // The whole viewer crate's directory, so the files' appearance
        // under dist/ reruns this script and the embed switches on.
        println!("cargo::rerun-if-changed={}", viewer.display());
        page_text
    };
    let newstroke = manifest
        .join("..")
        .join("kicad_core")
        .join("src")
        .join("font")
        .join("newstroke.txt");
    println!("cargo::rerun-if-changed={}", newstroke.display());
    let newstroke_bytes = fs::read(&newstroke).expect("the glyph table is readable");
    let newstroke_name = hashed_name("kiewer_viewer_newstroke", &newstroke_bytes, "txt");
    emit(
        "KIEWER_VIEWER_NEWSTROKE",
        &out,
        &newstroke_name,
        &newstroke_bytes,
    );
    named_once(
        &page_text,
        PAGE_SCRIPT_NEWSTROKE,
        "the page script fetches the glyph table",
    );
    let page_text = page_text.replace(PAGE_SCRIPT_NEWSTROKE, &format!("\"./{newstroke_name}\""));
    let page_name = hashed_name("kiewer_viewer_page", page_text.as_bytes(), "js");
    emit("KIEWER_VIEWER_PAGE", &out, &page_name, page_text.as_bytes());
}

/// Panics unless `text` names `name` exactly once, at the place where it
/// `does` what the message says (the import, the fetch), which the
/// rewrite to the hashed name replaces. Another count means the file
/// changed shape and the rewrite needs a look.
fn named_once(text: &str, name: &str, does: &str) {
    let mentions = text.matches(name).count();
    assert_eq!(
        mentions, 1,
        "{does} once, by {name}; this one names it {mentions} times, so the rewrite to the \
         hashed name needs a look"
    );
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
