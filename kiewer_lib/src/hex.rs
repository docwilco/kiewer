//! SHA-256 values as 32 bytes and 64 lowercase hex characters: the one
//! encoder and decoder the hash newtypes (blobs, trees, revisions) and
//! the session ids share, and the macro that makes a hash newtype of
//! them. One spelling per value: lowercase only, so one hash has one text
//! form in URLs, pages and rows.

use crate::error::{Error, Result};

/// 32 bytes as 64 lowercase hex characters.
pub(crate) fn encode(bytes: &[u8; 32]) -> String {
    use std::fmt::Write as _;
    let mut text = String::with_capacity(64);
    for byte in bytes {
        // Writing to a String cannot fail.
        let _ = write!(text, "{byte:02x}");
    }
    text
}

/// The 32 bytes 64 lowercase hex characters spell, and only that
/// spelling; `what` names the value in the error, which never echoes the
/// text (a session id is a secret).
///
/// # Errors
///
/// [`Error::Malformed`] for any other text.
pub(crate) fn decode(text: &str, what: &str) -> Result<[u8; 32]> {
    let malformed = || Error::Malformed(format!("not a {what}"));
    let digit = |b: u8| match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        _ => None,
    };
    if text.len() != 64 {
        return Err(malformed());
    }
    let mut out = [0u8; 32];
    for (slot, &[first, second]) in out.iter_mut().zip(text.as_bytes().as_chunks::<2>().0) {
        *slot = match (digit(first), digit(second)) {
            (Some(high), Some(low)) => (high << 4) | low,
            _ => return Err(malformed()),
        };
    }
    Ok(out)
}

/// A SHA-256 newtype: 32 bytes, shown as 64 lowercase hex characters,
/// parsed from the same, stored as `bytea`. The type's own constructors
/// (`of` for a hash computed here) are added beside it.
macro_rules! sha256_name {
    ($name:ident, $what:literal, $doc:literal) => {
        #[doc = $doc]
        #[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
        pub struct $name([u8; 32]);

        impl $name {
            /// The bytes, for a `bytea` parameter.
            #[must_use]
            pub fn as_bytes(&self) -> &[u8] {
                &self.0
            }

            /// The hash a `bytea` column holds.
            ///
            /// # Errors
            ///
            /// [`crate::Error::Malformed`] when the column is not 32 bytes,
            /// which the schema forbids.
            pub fn from_bytes(bytes: &[u8]) -> $crate::Result<Self> {
                bytes.try_into().map(Self).map_err(|_| {
                    $crate::Error::Malformed(format!(
                        "a {} is not 32 bytes: {} found",
                        $what,
                        bytes.len()
                    ))
                })
            }

            /// The short form: the first 8 hex characters
            /// (docs/v1-scope.md, Storage and history).
            #[must_use]
            pub fn short(&self) -> String {
                let mut text = self.to_string();
                text.truncate(8);
                text
            }
        }

        impl ::std::fmt::Display for $name {
            fn fmt(&self, f: &mut ::std::fmt::Formatter<'_>) -> ::std::fmt::Result {
                f.write_str(&$crate::hex::encode(&self.0))
            }
        }

        impl ::std::fmt::Debug for $name {
            fn fmt(&self, f: &mut ::std::fmt::Formatter<'_>) -> ::std::fmt::Result {
                write!(f, "{}({self})", stringify!($name))
            }
        }

        impl ::std::str::FromStr for $name {
            type Err = $crate::Error;

            fn from_str(text: &str) -> $crate::Result<Self> {
                $crate::hex::decode(text, $what).map(Self)
            }
        }
    };
}
pub(crate) use sha256_name;

#[cfg(test)]
mod tests {
    use test_case::test_case;

    use super::*;

    #[test]
    fn encode_and_decode_round_trip() {
        let bytes: [u8; 32] = core::array::from_fn(|i| u8::try_from(i * 8).unwrap());
        let text = encode(&bytes);
        assert_eq!(text.len(), 64);
        assert!(text.starts_with("0008101820"));
        assert_eq!(decode(&text, "hash").unwrap(), bytes);
    }

    #[test_case(""; "empty")]
    #[test_case("abc"; "short")]
    #[test_case(&"zz".repeat(32); "the length but not hex")]
    #[test_case(&"AB".repeat(32); "uppercase is not the one spelling")]
    #[test_case(&"+f".repeat(32); "a sign is not a digit")]
    fn other_text_is_refused_naming_the_value(text: &str) {
        let err = decode(text, "demo hash").unwrap_err();
        assert_eq!(err.to_string(), "not a demo hash");
    }
}
