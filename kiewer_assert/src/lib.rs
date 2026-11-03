//! Assertion macros the workspace's tests share.
//!
//! `assert_is_empty!(x)` in place of `assert!(x.is_empty())`, which
//! `clippy::assert_is_empty` refuses: a failure names the expression and
//! prints what it held, where the bare `assert!` prints only the condition.
//! Clippy's own rewrite, `assert_eq!` against an empty array, needs the
//! element type spelled out at every site (#102).

/// Asserts that `$value.is_empty()`, printing the value when it is not.
/// The value is evaluated once and borrowed; it needs `Debug`.
#[macro_export]
macro_rules! assert_is_empty {
    ($value:expr $(,)?) => {
        match &$value {
            value => {
                if !value.is_empty() {
                    ::core::panic!(
                        "assertion failed: `{}` is empty\n  it holds: {:?}",
                        ::core::stringify!($value),
                        value,
                    );
                }
            }
        }
    };
}

/// Asserts that `$value.is_empty()` is false. The value is evaluated once
/// and borrowed.
#[macro_export]
macro_rules! assert_is_not_empty {
    ($value:expr $(,)?) => {
        if $value.is_empty() {
            ::core::panic!(
                "assertion failed: `{}` is not empty",
                ::core::stringify!($value),
            );
        }
    };
}

#[cfg(test)]
mod tests {
    #[test]
    fn an_empty_value_passes() {
        assert_is_empty!(Vec::<u8>::new());
        assert_is_empty!("");
    }

    #[test]
    #[should_panic(expected = "assertion failed: `vec![1, 2]` is empty\n  it holds: [1, 2]")]
    fn a_value_with_items_fails_naming_the_expression_and_its_items() {
        assert_is_empty!(vec![1, 2]);
    }

    #[test]
    fn a_value_with_items_passes_not_empty() {
        assert_is_not_empty!([1]);
    }

    #[test]
    #[should_panic(expected = "assertion failed: `String::new()` is not empty")]
    fn an_empty_value_fails_not_empty_naming_the_expression() {
        assert_is_not_empty!(String::new());
    }
}
