//! Output formats: `json`, `python`/`python3`, `CSV` and the default "broken" `csv`.
//!
//! The encodings mirror the original Livestatus byte for byte (string escaping,
//! float formatting, row framing), so existing parsers keep working.

use crate::value::{format_double, Value};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputFormat {
    /// `OutputFormat: csv` (and the default): separator-based, no quoting.
    BrokenCsv,
    /// `OutputFormat: CSV`: RFC 4180-ish.
    Csv,
    Json,
    Python3,
}

impl OutputFormat {
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "CSV" => Self::Csv,
            "csv" => Self::BrokenCsv,
            "json" => Self::Json,
            "python" | "python3" => Self::Python3,
            _ => return None,
        })
    }
}

/// `Separators:` header (dataset, field, list, host/service).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Separators {
    pub dataset: u8,
    pub field: u8,
    pub list: u8,
    pub host_service: u8,
}

impl Default for Separators {
    fn default() -> Self {
        Separators { dataset: b'\n', field: b';', list: b',', host_service: b'|' }
    }
}

/// Render a complete result set.
pub fn render(format: OutputFormat, seps: Separators, rows: &[Vec<Value>]) -> Vec<u8> {
    let mut out = Vec::with_capacity(rows.len() * 64 + 4);
    match format {
        OutputFormat::Json | OutputFormat::Python3 => {
            let py = format == OutputFormat::Python3;
            out.push(b'[');
            for (i, row) in rows.iter().enumerate() {
                if i > 0 {
                    out.extend_from_slice(b",\n");
                }
                out.push(b'[');
                for (j, v) in row.iter().enumerate() {
                    if j > 0 {
                        out.push(b',');
                    }
                    structured(&mut out, v, py);
                }
                out.push(b']');
            }
            out.extend_from_slice(b"]\n");
        }
        OutputFormat::BrokenCsv => {
            for row in rows {
                for (j, v) in row.iter().enumerate() {
                    if j > 0 {
                        out.push(seps.field);
                    }
                    broken_csv(&mut out, v, seps, false);
                }
                out.push(seps.dataset);
            }
        }
        OutputFormat::Csv => {
            for row in rows {
                for (j, v) in row.iter().enumerate() {
                    if j > 0 {
                        out.push(b',');
                    }
                    out.push(b'"');
                    csv(&mut out, v, false);
                    out.push(b'"');
                }
                out.extend_from_slice(b"\r\n");
            }
        }
    }
    out
}

fn is_boring(ch: u32) -> bool {
    (32..=127).contains(&ch) && ch != u32::from(b'"') && ch != u32::from(b'\\')
}

fn hex(out: &mut Vec<u8>, prefix: char, width: usize, n: u32) {
    out.extend_from_slice(format!("\\{prefix}{n:0width$x}").as_bytes());
}

fn unicode_char(out: &mut Vec<u8>, ch: u32, surrogates: bool) {
    if ch < 0x10000 {
        hex(out, 'u', 4, ch);
    } else if surrogates {
        hex(out, 'u', 4, 0xd800 | (((ch - 0x10000) >> 10) & 0x3ff));
        hex(out, 'u', 4, 0xdc00 | ((ch - 0x10000) & 0x3ff));
    } else {
        hex(out, 'U', 8, ch);
    }
}

fn quoted_str(out: &mut Vec<u8>, s: &str, py: bool) {
    out.push(b'"');
    for ch in s.chars() {
        let c = ch as u32;
        if is_boring(c) {
            out.push(c as u8);
        } else {
            unicode_char(out, c, !py);
        }
    }
    out.push(b'"');
}

fn structured(out: &mut Vec<u8>, v: &Value, py: bool) {
    match v {
        Value::Null => out.extend_from_slice(if py { b"None" } else { b"null" }),
        Value::Int(i) => out.extend_from_slice(i.to_string().as_bytes()),
        Value::Float(f) if f.is_nan() => out.extend_from_slice(if py { b"None" } else { b"null" }),
        Value::Float(f) => out.extend_from_slice(format_double(*f).as_bytes()),
        Value::Str(s) => quoted_str(out, s, py),
        Value::Blob(b) => {
            if py {
                out.extend_from_slice(b"b\"");
                for &byte in b {
                    if is_boring(u32::from(byte)) {
                        out.push(byte);
                    } else {
                        hex(out, 'x', 2, u32::from(byte));
                    }
                }
                out.push(b'"');
            } else {
                // JSON: the blob is interpreted as Latin-1
                out.push(b'"');
                for &byte in b {
                    if is_boring(u32::from(byte)) {
                        out.push(byte);
                    } else {
                        unicode_char(out, u32::from(byte), true);
                    }
                }
                out.push(b'"');
            }
        }
        Value::List(items) => {
            out.push(b'[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(b',');
                }
                structured(out, item, py);
            }
            out.push(b']');
        }
        Value::Dict(items) => {
            out.push(b'{');
            for (i, (k, val)) in items.iter().enumerate() {
                if i > 0 {
                    out.push(b',');
                }
                quoted_str(out, k, py);
                out.push(b':');
                structured(out, val, py);
            }
            out.push(b'}');
        }
    }
}

fn broken_csv(out: &mut Vec<u8>, v: &Value, seps: Separators, in_list: bool) {
    match v {
        Value::Null => {}
        Value::Int(i) => out.extend_from_slice(i.to_string().as_bytes()),
        Value::Float(f) => out.extend_from_slice(format_double(*f).as_bytes()),
        Value::Str(s) => out.extend_from_slice(s.as_bytes()),
        Value::Blob(b) => out.extend_from_slice(b),
        Value::List(items) => {
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(if in_list { seps.host_service } else { seps.list });
                }
                broken_csv(out, item, seps, true);
            }
        }
        Value::Dict(items) => {
            for (i, (k, val)) in items.iter().enumerate() {
                if i > 0 {
                    out.push(seps.list);
                }
                out.extend_from_slice(k.as_bytes());
                out.push(seps.host_service);
                broken_csv(out, val, seps, true);
            }
        }
    }
}

fn csv_escaped(out: &mut Vec<u8>, bytes: &[u8]) {
    for &b in bytes {
        if b == b'"' {
            out.extend_from_slice(b"\"\"");
        } else {
            out.push(b);
        }
    }
}

fn csv(out: &mut Vec<u8>, v: &Value, in_list: bool) {
    match v {
        Value::Null => {}
        Value::Int(i) => out.extend_from_slice(i.to_string().as_bytes()),
        Value::Float(f) => out.extend_from_slice(format_double(*f).as_bytes()),
        Value::Str(s) => csv_escaped(out, s.as_bytes()),
        Value::Blob(b) => csv_escaped(out, b),
        Value::List(items) => {
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(if in_list { b'|' } else { b',' });
                }
                csv(out, item, true);
            }
        }
        Value::Dict(items) => {
            for (i, (k, val)) in items.iter().enumerate() {
                if i > 0 {
                    out.push(b',');
                }
                csv_escaped(out, k.as_bytes());
                out.push(b'|');
                csv(out, val, true);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(b: Vec<u8>) -> String {
        String::from_utf8(b).unwrap()
    }

    #[test]
    fn python3_framing_and_escaping() {
        let rows = vec![
            vec![Value::str("web01"), Value::Int(0), Value::Float(0.018), Value::Blob(vec![])],
            vec![Value::str("ä\"\\\n😀"), Value::Null, Value::Float(1.0), Value::Blob(vec![0, b'a'])],
        ];
        assert_eq!(
            s(render(OutputFormat::Python3, Separators::default(), &rows)),
            "[[\"web01\",0,0.018,b\"\"],\n[\"\\u00e4\\u0022\\u005c\\u000a\\U0001f600\",None,1,b\"\\x00a\"]]\n"
        );
    }

    #[test]
    fn json_surrogates_and_empty() {
        let rows = vec![vec![Value::str("😀"), Value::Null]];
        assert_eq!(s(render(OutputFormat::Json, Separators::default(), &rows)), "[[\"\\ud83d\\ude00\",null]]\n");
        assert_eq!(s(render(OutputFormat::Json, Separators::default(), &[])), "[]\n");
    }

    #[test]
    fn nested_structures() {
        let rows = vec![vec![
            Value::List(vec![Value::List(vec![Value::str("Uptime"), Value::Int(0)])]),
            Value::str_dict([("a", "b")]),
        ]];
        assert_eq!(
            s(render(OutputFormat::Python3, Separators::default(), &rows)),
            "[[[[\"Uptime\",0]],{\"a\":\"b\"}]]\n"
        );
        assert_eq!(s(render(OutputFormat::BrokenCsv, Separators::default(), &rows)), "Uptime|0;a|b\n");
    }

    #[test]
    fn broken_csv_lists() {
        let rows = vec![vec![Value::str("web01"), Value::str_list(["a", "b"])], vec![Value::str("x"), Value::Null]];
        assert_eq!(s(render(OutputFormat::BrokenCsv, Separators::default(), &rows)), "web01;a,b\nx;\n");
    }

    #[test]
    fn rfc_csv() {
        let rows = vec![vec![Value::str("a\"b"), Value::Int(1)]];
        assert_eq!(s(render(OutputFormat::Csv, Separators::default(), &rows)), "\"a\"\"b\",\"1\"\r\n");
    }
}
