//! Tokenizer for Nagios object definition files.
//!
//! Supported syntax (compatible with Nagios Core 3.x):
//! * `define <type> { ... }` blocks, opening brace on the same line
//! * one `key value` pair per line, value may contain spaces
//! * comments: lines starting with `#` or `;`, inline comments after an unescaped `;`
//! * `\;` for a literal semicolon
//! * backslash line continuation (a trailing `\` joins the next line)

use std::path::{Path, PathBuf};

use crate::ConfigError;

/// One `define` block before template resolution.
#[derive(Debug, Clone)]
pub struct RawObject {
    pub kind: String,
    /// Attributes in file order. Later duplicates override earlier ones.
    pub attrs: Vec<(String, String)>,
    pub file: PathBuf,
    pub line: usize,
}

impl RawObject {
    pub fn get(&self, key: &str) -> Option<&str> {
        self.attrs.iter().rev().find(|(k, _)| k == key).map(|(_, v)| v.as_str())
    }
}

/// Strip an inline `;` comment, honouring `\;` escapes.
fn strip_comment(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\\' if chars.peek() == Some(&';') => {
                out.push(';');
                chars.next();
            }
            ';' => break,
            _ => out.push(c),
        }
    }
    out
}

/// Join backslash-continued lines. Returns `(logical_line, first_physical_line_number)`.
fn logical_lines(text: &str) -> Vec<(String, usize)> {
    let mut result = Vec::new();
    let mut pending: Option<(String, usize)> = None;
    for (idx, raw) in text.lines().enumerate() {
        let lineno = idx + 1;
        let trimmed_end = raw.trim_end();
        let (content, continues) = match trimmed_end.strip_suffix('\\') {
            // `\\` at end of line is an escaped backslash, not a continuation
            Some(rest) if !rest.ends_with('\\') => (rest.trim_end(), true),
            _ => (trimmed_end, false),
        };
        match pending.as_mut() {
            Some((buf, _)) => {
                buf.push(' ');
                buf.push_str(content.trim_start());
            }
            None => pending = Some((content.to_string(), lineno)),
        }
        if !continues {
            result.extend(pending.take());
        }
    }
    result.extend(pending);
    result
}

/// Parse the text of one object configuration file.
pub fn parse_object_text(text: &str, file: &Path) -> Result<Vec<RawObject>, ConfigError> {
    let mut objects = Vec::new();
    let mut current: Option<RawObject> = None;
    let syntax = |line: usize, message: String| ConfigError::Syntax { path: file.to_path_buf(), line, message };

    for (line, lineno) in logical_lines(text) {
        let trimmed = line.trim_start();
        if trimmed.is_empty() || trimmed.starts_with('#') || trimmed.starts_with(';') {
            continue;
        }
        let content = strip_comment(trimmed);
        let content = content.trim();
        if content.is_empty() {
            continue;
        }

        if let Some(obj) = current.as_mut() {
            if content == "}" {
                objects.push(current.take().expect("current object exists"));
                continue;
            }
            // A closing brace may also end a value line: `key value }`
            let (content, closes) = match content.strip_suffix('}') {
                Some(rest) if rest.ends_with(char::is_whitespace) => (rest.trim_end(), true),
                _ => (content, false),
            };
            let (key, value) = match content.split_once(|c: char| c.is_ascii_whitespace()) {
                Some((k, v)) => (k, v.trim()),
                None => (content, ""),
            };
            obj.attrs.push((key.to_string(), value.to_string()));
            if closes {
                objects.push(current.take().expect("current object exists"));
            }
            continue;
        }

        let Some(rest) = content.strip_prefix("define") else {
            return Err(syntax(lineno, format!("unexpected text outside of definition: '{content}'")));
        };
        let rest = rest.trim();
        let Some(kind) = rest.strip_suffix('{') else {
            return Err(syntax(lineno, "expected '{' after object type".into()));
        };
        let kind = kind.trim();
        if kind.is_empty() {
            return Err(syntax(lineno, "missing object type after 'define'".into()));
        }
        current = Some(RawObject { kind: kind.to_string(), attrs: Vec::new(), file: file.to_path_buf(), line: lineno });
    }

    if let Some(obj) = current {
        return Err(syntax(obj.line, format!("unterminated '{}' definition", obj.kind)));
    }
    Ok(objects)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(s: &str) -> Vec<RawObject> {
        parse_object_text(s, Path::new("test.cfg")).unwrap()
    }

    #[test]
    fn basic_definition() {
        let objs = parse("# comment\ndefine host {\n  host_name  web01 ; trailing\n  alias a b c\n}\n");
        assert_eq!(objs.len(), 1);
        assert_eq!(objs[0].kind, "host");
        assert_eq!(objs[0].get("host_name"), Some("web01"));
        assert_eq!(objs[0].get("alias"), Some("a b c"));
    }

    #[test]
    fn escaped_semicolon_and_continuation() {
        let objs = parse("define command{\n command_name x\n command_line A='$X$' \\\n   B=1 echo a\\;b\n}\n");
        assert_eq!(objs[0].get("command_line"), Some("A='$X$' B=1 echo a;b"));
    }

    #[test]
    fn define_without_space() {
        let objs = parse("define service{\nhost_name h\n}");
        assert_eq!(objs[0].kind, "service");
    }

    #[test]
    fn unterminated_is_error() {
        assert!(parse_object_text("define host {\n host_name x\n", Path::new("x")).is_err());
    }

    #[test]
    fn empty_value() {
        let objs = parse("define host {\n  _ADDRESS_6\n  host_name h\n}");
        assert_eq!(objs[0].get("_ADDRESS_6"), Some(""));
    }
}
