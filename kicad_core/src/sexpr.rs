//! S-expression tokenizer and reader.
//!
//! KiCad files are one S-expression: lists in parentheses holding bare
//! atoms (`segment`, `1.25`, `yes`), quoted strings with backslash escapes,
//! and nested lists. [`read`] turns the text into a [`Node`] tree whose
//! every lookup is a `Result`: a missing child, a value that is not a
//! number, or an index past the end is an [`Error`] carrying the position,
//! never a panic. The tree keeps the byte offset and line of every node so
//! the parser above it can say where a field went wrong.

use crate::error::{Error, Result};

/// Where in the source a token or list starts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Position {
    /// Byte offset into the source.
    pub offset: usize,
    /// 1-based line.
    pub line: u32,
}

impl Position {
    /// An error found at this position.
    #[must_use]
    pub fn error(self, message: impl Into<String>) -> Error {
        Error::Malformed {
            offset: self.offset,
            line: self.line,
            message: message.into(),
        }
    }
}

/// One bare or quoted atom.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Atom {
    /// Where the atom starts.
    pub position: Position,
    /// The text, with quotes removed and escapes resolved.
    pub text: String,
    /// Whether the text was written in quotes. `(layer "F.Cu" knockout)`
    /// distinguishes the layer name from the flag by this.
    pub quoted: bool,
}

/// An atom or a list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Value {
    /// A bare or quoted atom.
    Atom(Atom),
    /// A parenthesised list.
    List(Node),
}

impl Value {
    fn as_atom(&self) -> Option<&Atom> {
        match self {
            Value::Atom(atom) => Some(atom),
            Value::List(_) => None,
        }
    }

    fn as_list(&self) -> Option<&Node> {
        match self {
            Value::List(node) => Some(node),
            Value::Atom(_) => None,
        }
    }
}

/// A parenthesised list: a head atom naming what it is, then its values.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Node {
    /// Where the opening parenthesis is.
    pub position: Position,
    /// Everything between the parentheses, the head included.
    pub items: Vec<Value>,
}

impl Node {
    /// An error found at this list.
    #[must_use]
    pub fn error(&self, message: impl Into<String>) -> Error {
        self.position.error(message)
    }

    /// The head: the first item, which has to be a bare atom.
    ///
    /// # Errors
    /// The list is empty or starts with a string or a list.
    pub fn head(&self) -> Result<&str> {
        match self.items.first() {
            Some(Value::Atom(atom)) if !atom.quoted => Ok(&atom.text),
            _ => Err(self.error("expected a name after (")),
        }
    }

    /// Everything after the head.
    #[must_use]
    pub fn args(&self) -> &[Value] {
        self.items.get(1..).unwrap_or_default()
    }

    fn describe(&self) -> String {
        self.head()
            .map_or_else(|_| "list".to_owned(), |h| format!("({h} ...)"))
    }

    /// The `index`-th value after the head when it is an atom.
    #[must_use]
    pub fn atom_opt(&self, index: usize) -> Option<&str> {
        self.args()
            .get(index)
            .and_then(Value::as_atom)
            .map(|atom| atom.text.as_str())
    }

    /// The `index`-th value after the head, which has to be an atom.
    ///
    /// # Errors
    /// There is no such value, or it is a list.
    pub fn atom(&self, index: usize) -> Result<&str> {
        self.atom_at(index).map(|atom| atom.text.as_str())
    }

    /// The `index`-th value after the head as the [`Atom`] it was read as,
    /// for the callers that care whether it was quoted.
    ///
    /// # Errors
    /// There is no such value, or it is a list.
    pub fn atom_at(&self, index: usize) -> Result<&Atom> {
        match self.args().get(index) {
            Some(Value::Atom(atom)) => Ok(atom),
            Some(Value::List(list)) => Err(list.error(format!(
                "expected value {} of {} to be an atom, found a list",
                index + 1,
                self.describe()
            ))),
            None => Err(self.error(format!(
                "expected at least {} value(s) in {}, found {}",
                index + 1,
                self.describe(),
                self.args().len()
            ))),
        }
    }

    /// The `index`-th value after the head as a finite number.
    ///
    /// # Errors
    /// There is no such value, or it is not a finite decimal number.
    pub fn number(&self, index: usize) -> Result<f64> {
        let text = self.atom(index)?;
        match text.parse::<f64>() {
            Ok(value) if value.is_finite() => Ok(value),
            _ => Err(self.error(format!(
                "expected value {} of {} to be a number, found {text:?}",
                index + 1,
                self.describe()
            ))),
        }
    }

    /// The `index`-th value after the head as an integer.
    ///
    /// # Errors
    /// There is no such value, or it is not an integer.
    pub fn integer(&self, index: usize) -> Result<i64> {
        let text = self.atom(index)?;
        text.parse::<i64>().map_err(|_| {
            self.error(format!(
                "expected value {} of {} to be an integer, found {text:?}",
                index + 1,
                self.describe()
            ))
        })
    }

    /// Whether a bare atom equal to `flag` follows the head, as `knockout`
    /// in `(layer "F.SilkS" knockout)`.
    #[must_use]
    pub fn has_flag(&self, flag: &str) -> bool {
        self.args()
            .iter()
            .filter_map(Value::as_atom)
            .any(|atom| !atom.quoted && atom.text == flag)
    }

    /// Every list among the values.
    pub fn lists(&self) -> impl Iterator<Item = &Node> {
        self.args().iter().filter_map(Value::as_list)
    }

    /// Every list among the values whose head is `name`.
    pub fn children<'a>(&'a self, name: &'a str) -> impl Iterator<Item = &'a Node> + 'a {
        self.lists()
            .filter(move |node| node.head().is_ok_and(|h| h == name))
    }

    /// The first list among the values whose head is `name`.
    #[must_use]
    pub fn child(&self, name: &str) -> Option<&Node> {
        self.lists()
            .find(|node| node.head().is_ok_and(|h| h == name))
    }

    /// The first list among the values whose head is `name`.
    ///
    /// # Errors
    /// There is none.
    pub fn require(&self, name: &str) -> Result<&Node> {
        self.child(name)
            .ok_or_else(|| self.error(format!("expected ({name} ...) in {}", self.describe())))
    }
}

/// Reads one S-expression from `source`.
///
/// # Errors
/// The text is empty, has an unterminated string, has unbalanced
/// parentheses, or holds anything after the one top-level list.
pub fn read(source: &str) -> Result<Node> {
    Reader::new(source).read_document()
}

/// The deepest nesting [`read`] accepts.
pub const MAX_DEPTH: usize = 128;

struct Reader<'a> {
    chars: core::iter::Peekable<core::str::CharIndices<'a>>,
    len: usize,
    line: u32,
}

impl<'a> Reader<'a> {
    fn new(source: &'a str) -> Self {
        Self {
            chars: source.char_indices().peekable(),
            len: source.len(),
            line: 1,
        }
    }

    fn here(&mut self) -> Position {
        let offset = self.chars.peek().map_or(self.len, |&(offset, _)| offset);
        Position {
            offset,
            line: self.line,
        }
    }

    fn bump(&mut self) -> Option<(usize, char)> {
        let next = self.chars.next();
        if let Some((_, '\n')) = next {
            self.line += 1;
        }
        next
    }

    fn skip_whitespace(&mut self) {
        while self.chars.peek().is_some_and(|&(_, c)| c.is_whitespace()) {
            self.bump();
        }
    }

    fn read_document(&mut self) -> Result<Node> {
        self.skip_whitespace();
        let start = self.here();
        match self.bump() {
            Some((_, '(')) => {}
            Some((_, c)) => {
                return Err(start.error(format!("expected ( at the start, found {c:?}")));
            }
            None => return Err(start.error("expected ( at the start, found the end of the text")),
        }
        let root = self.read_list(start)?;
        self.skip_whitespace();
        let after = self.here();
        match self.bump() {
            None => Ok(root),
            Some((_, c)) => Err(after.error(format!(
                "expected the end of the text after the top-level list, found {c:?}"
            ))),
        }
    }

    /// Reads the items of a list whose `(` has been consumed, through its `)`.
    fn read_list(&mut self, open: Position) -> Result<Node> {
        // An explicit stack with a depth cap rather than recursion: a file
        // of a hundred thousand nested parentheses must be an error, not a
        // stack overflow here or in the tree's derived Drop, Debug and
        // PartialEq, which recurse. KiCad nests about ten deep.
        let mut stack: Vec<Node> = vec![Node {
            position: open,
            items: Vec::new(),
        }];
        loop {
            self.skip_whitespace();
            let position = self.here();
            let Some((_, c)) = self.bump() else {
                let open = stack.last().map_or(open, |node| node.position);
                return Err(position.error(format!(
                    "expected ) to close the list opened at line {}, found the end of the text",
                    open.line
                )));
            };
            match c {
                '(' => {
                    if stack.len() >= MAX_DEPTH {
                        return Err(
                            position.error(format!("lists nested deeper than {MAX_DEPTH} levels"))
                        );
                    }
                    stack.push(Node {
                        position,
                        items: Vec::new(),
                    });
                }
                ')' => {
                    let Some(done) = stack.pop() else {
                        return Err(position.error("unexpected )"));
                    };
                    match stack.last_mut() {
                        Some(parent) => parent.items.push(Value::List(done)),
                        None => return Ok(done),
                    }
                }
                '"' => {
                    let text = self.read_string(position)?;
                    let atom = Atom {
                        position,
                        text,
                        quoted: true,
                    };
                    Self::push_atom(&mut stack, atom)?;
                }
                first => {
                    let mut text = String::new();
                    text.push(first);
                    while let Some(&(_, c)) = self.chars.peek() {
                        if c.is_whitespace() || matches!(c, '(' | ')' | '"') {
                            break;
                        }
                        text.push(c);
                        self.bump();
                    }
                    let atom = Atom {
                        position,
                        text,
                        quoted: false,
                    };
                    Self::push_atom(&mut stack, atom)?;
                }
            }
        }
    }

    fn push_atom(stack: &mut [Node], atom: Atom) -> Result<()> {
        let Some(top) = stack.last_mut() else {
            return Err(atom.position.error("expected ( before a value"));
        };
        top.items.push(Value::Atom(atom));
        Ok(())
    }

    /// Reads a quoted string whose opening quote has been consumed.
    fn read_string(&mut self, open: Position) -> Result<String> {
        let mut text = String::new();
        loop {
            let Some((_, c)) = self.bump() else {
                return Err(
                    open.error("expected \" to close the string, found the end of the text")
                );
            };
            match c {
                '"' => return Ok(text),
                '\\' => {
                    let Some((_, escaped)) = self.bump() else {
                        return Err(open
                            .error("expected \" to close the string, found the end of the text"));
                    };
                    text.push(match escaped {
                        'n' => '\n',
                        't' => '\t',
                        'r' => '\r',
                        other => other,
                    });
                }
                other => text.push(other),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_case::test_case;

    fn err(source: &str) -> String {
        read(source)
            .expect_err("the source is malformed")
            .to_string()
    }

    #[test]
    fn reads_head_values_and_children() {
        let node = read("(segment (start 1 2.5) (end -3 4) (layer \"F.Cu\") (net \"GND\"))")
            .expect("a well-formed list reads");
        assert_eq!(node.head(), Ok("segment"));
        let start = node.require("start").expect("start is present");
        assert_eq!(start.number(0), Ok(1.0));
        assert_eq!(start.number(1), Ok(2.5));
        assert_eq!(node.require("end").and_then(|n| n.number(0)), Ok(-3.0));
        assert_eq!(node.require("layer").and_then(|n| n.atom(0)), Ok("F.Cu"));
        assert_eq!(node.child("width"), None);
        assert_eq!(node.children("start").count(), 1);
    }

    #[test]
    fn resolves_string_escapes_and_keeps_quoting() {
        let node = read(r#"(gr_text "a \"b\"\nc\\d" knockout)"#).expect("reads");
        assert_eq!(node.atom(0), Ok("a \"b\"\nc\\d"));
        assert!(node.has_flag("knockout"));
        assert!(!node.has_flag("a \"b\"\nc\\d"));
    }

    #[test]
    fn a_quoted_flag_is_not_a_flag() {
        let node = read(r#"(layer "knockout")"#).expect("reads");
        assert!(!node.has_flag("knockout"));
        assert_eq!(node.atom(0), Ok("knockout"));
    }

    #[test]
    fn tolerates_crlf_and_tabs_and_counts_lines() {
        let node = read("(a\r\n\t(b 1)\r\n\t(c 2))").expect("reads");
        assert_eq!(node.require("c").map(|n| n.position.line), Ok(3));
        assert_eq!(node.require("c").map(|n| n.position.offset), Ok(13));
    }

    #[test]
    fn positions_name_the_offending_value() {
        let node = read("(a\n (b 1)\n (c x))").expect("reads");
        let error = node
            .require("c")
            .and_then(|n| n.number(0))
            .expect_err("x is not a number");
        assert_eq!(
            error.to_string(),
            "line 3 (byte 11): expected value 1 of (c ...) to be a number, found \"x\""
        );
    }

    #[test]
    fn missing_child_names_the_parent_and_the_child() {
        let node = read("(via (at 1 2))").expect("reads");
        assert_eq!(
            node.require("size").expect_err("no size").to_string(),
            "line 1 (byte 0): expected (size ...) in (via ...)"
        );
    }

    #[test]
    fn index_past_the_end_is_an_error() {
        let node = read("(at 1)").expect("reads");
        assert_eq!(
            node.number(1).expect_err("one value only").to_string(),
            "line 1 (byte 0): expected at least 2 value(s) in (at ...), found 1"
        );
        assert_eq!(node.atom_opt(1), None);
        assert_eq!(node.atom_opt(0), Some("1"));
    }

    #[test]
    fn a_list_where_an_atom_is_expected_is_an_error() {
        let node = read("(at (x 1))").expect("reads");
        assert!(
            node.atom(0)
                .expect_err("a list")
                .to_string()
                .contains("found a list")
        );
    }

    #[test_case("1e999"; "infinite")]
    #[test_case("nan"; "not a number")]
    #[test_case("1.2.3"; "two points")]
    #[test_case("0x10"; "hex")]
    #[test_case("-"; "sign only")]
    fn number_rejects_what_is_not_finite_decimal(text: &str) {
        let node = read(&format!("(w {text})")).expect("reads");
        assert!(node.number(0).is_err(), "{text} parsed as a number");
    }

    #[test]
    fn a_quoted_number_is_still_a_number() {
        let node = read("(w \"1.5\")").expect("reads");
        assert_eq!(node.number(0), Ok(1.5));
    }

    #[test]
    fn integer_rejects_fractions() {
        let node = read("(version 2024.5)").expect("reads");
        assert!(node.integer(0).is_err());
        assert_eq!(
            read("(version 20241229)").and_then(|n| n.integer(0)),
            Ok(20_241_229)
        );
    }

    #[test]
    fn head_of_an_empty_or_quoted_list_is_an_error() {
        assert!(read("()").expect("reads").head().is_err());
        assert!(read("(\"a\" 1)").expect("reads").head().is_err());
    }

    #[test_case("", "line 1 (byte 0): expected ( at the start, found the end of the text"; "empty")]
    #[test_case("   \n ", "line 2 (byte 5): expected ( at the start, found the end of the text"; "blank")]
    #[test_case("kicad_pcb", "line 1 (byte 0): expected ( at the start, found 'k'"; "no paren")]
    #[test_case("(a (b 1)", "line 1 (byte 8): expected ) to close the list opened at line 1, found the end of the text"; "unclosed")]
    #[test_case("(a\n(b\n", "line 3 (byte 6): expected ) to close the list opened at line 2, found the end of the text"; "unclosed inner")]
    #[test_case("(a \"xy", "line 1 (byte 3): expected \" to close the string, found the end of the text"; "unterminated string")]
    #[test_case("(a \"xy\\", "line 1 (byte 3): expected \" to close the string, found the end of the text"; "escape at the end")]
    #[test_case("(a) (b)", "line 1 (byte 4): expected the end of the text after the top-level list, found '('"; "two lists")]
    #[test_case("(a))", "line 1 (byte 3): expected the end of the text after the top-level list, found ')'"; "stray close")]
    fn malformed_text_is_an_error_with_a_position(source: &str, message: &str) {
        assert_eq!(err(source), message);
    }

    #[test]
    fn deep_nesting_is_an_error_not_a_stack_overflow() {
        let nested = |depth: usize| format!("{}{}", "(".repeat(depth), ")".repeat(depth));
        assert!(read(&nested(MAX_DEPTH)).is_ok());
        assert_eq!(
            err(&nested(200_000)),
            "line 1 (byte 128): lists nested deeper than 128 levels"
        );
        assert!(read(&"(".repeat(200_000)).is_err());
    }
}
