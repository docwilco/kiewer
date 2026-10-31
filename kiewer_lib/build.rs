// The target triple and the profile, for the build line: cargo hands them
// to a build script and to nothing else, so this script passes them on as
// environment variables the crate reads with env!.
fn main() {
    for (from, to) in [("TARGET", "KIEWER_TARGET"), ("PROFILE", "KIEWER_PROFILE")] {
        let value =
            std::env::var(from).unwrap_or_else(|_| panic!("cargo sets {from} for a build script"));
        println!("cargo:rustc-env={to}={value}");
    }
    println!("cargo:rerun-if-changed=build.rs");
}
