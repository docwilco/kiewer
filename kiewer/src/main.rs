//! The kiewer server executable.

fn main() {
    // The build line: the Version and the short hash of the commit the
    // release build was made from (release-conventions).
    println!(
        "kiewer {}+{}",
        env!("CARGO_PKG_VERSION"),
        option_env!("KIEWER_BUILD").unwrap_or("dev")
    );
}
