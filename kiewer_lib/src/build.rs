//! What this build is: the line the executable prints first.

/// The Version, from the workspace manifest.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// The short hash of the commit the release build was made from, set by
/// scripts/release-build.sh as `KIEWER_BUILD`; `dev` for a plain
/// `cargo run`.
pub const BUILD: &str = match option_env!("KIEWER_BUILD") {
    Some(build) => build,
    None => "dev",
};

/// The target triple the crate was compiled for (from build.rs).
pub const TARGET: &str = env!("KIEWER_TARGET");

/// The cargo profile the crate was compiled under (from build.rs).
pub const PROFILE: &str = env!("KIEWER_PROFILE");

/// The build line: `kiewer <Version>+<short hash> (<target>, <profile>)`.
/// The first part is the shape release-conventions asks every deployed
/// program to log first; the rest tells a `cargo run` from a release.
#[must_use]
pub fn line() -> String {
    format!("kiewer {VERSION}+{BUILD} ({TARGET}, {PROFILE})")
}

#[cfg(test)]
mod tests {
    use kiewer_assert::assert_is_not_empty;

    use super::*;

    #[test]
    fn line_has_the_release_shape_first() {
        let line = line();
        let (release, rest) = line
            .split_once(" (")
            .expect("the target and profile follow in parentheses");
        assert_eq!(release, format!("kiewer {VERSION}+{BUILD}"));
        assert_eq!(rest, format!("{TARGET}, {PROFILE})"));
        assert_is_not_empty!(TARGET);
        assert!(matches!(PROFILE, "debug" | "release"));
    }
}
