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
use std::env;
use std::path::PathBuf;

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
        println!("cargo::rustc-env=KIEWER_VIEWER_JS={}", script.display());
        println!("cargo::rustc-env=KIEWER_VIEWER_WASM={}", wasm.display());
        println!("cargo::rerun-if-changed={}", script.display());
        println!("cargo::rerun-if-changed={}", wasm.display());
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
