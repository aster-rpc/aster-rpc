//! JSON exactly as Python's `json.dumps` writes it. The @aster service signs
//! and hashes Python-serialised JSON, so a client must reproduce those bytes:
//! request signatures cover `json.dumps(payload, sort_keys=True,
//! separators=(",", ":"), ensure_ascii=False)` and contract IDs hash
//! `json.dumps(asdict(manifest))` (default separators, ASCII escapes, object
//! order as parsed).
//!
//! [`PyJson`] keeps object order from its input, integers apart from floats
//! and integers of any size exactly. It parses with its own `json.loads`
//! equivalent, so neither serde_json's `preserve_order` nor its number
//! handling (`arbitrary_precision`) matters.

use std::fmt::Write;

use anyhow::{Result, anyhow};

#[derive(Debug, Clone, PartialEq)]
pub enum PyJson {
    Null,
    Bool(bool),
    Int(i128),
    /// An integer beyond `i128`, as its decimal digits (Python ints are
    /// unbounded).
    BigInt(String),
    Float(f64),
    Str(String),
    Array(Vec<PyJson>),
    /// Keys in input order.
    Object(Vec<(String, PyJson)>),
}

#[derive(Debug, Clone, Copy)]
pub struct DumpOptions {
    pub sort_keys: bool,
    /// `(",", ":")` when true, Python's default `(", ", ": ")` otherwise.
    pub compact: bool,
    pub ensure_ascii: bool,
    /// Python's `indent`: one item per line, nested this many spaces deeper,
    /// with `","` between items whatever `compact` says.
    pub indent: Option<usize>,
}

impl DumpOptions {
    /// What the @aster service signs.
    pub const SIGNING: Self = Self {
        sort_keys: true,
        compact: true,
        ensure_ascii: false,
        indent: None,
    };
    /// `json.dumps(value)` with no arguments.
    pub const PYTHON_DEFAULT: Self = Self {
        sort_keys: false,
        compact: false,
        ensure_ascii: true,
        indent: None,
    };
    /// `json.dumps(value, indent=2, ensure_ascii=False)`, for files people read.
    pub const PRETTY: Self = Self {
        sort_keys: false,
        compact: false,
        ensure_ascii: false,
        indent: Some(2),
    };
}

impl PyJson {
    /// Python's `json.loads`: standard JSON plus `NaN`, `Infinity` and
    /// `-Infinity`; a repeated key keeps the last value in the first key's
    /// position. Lone UTF-16 surrogates, which a Rust string cannot hold, are
    /// refused.
    pub fn parse(text: &str) -> Result<Self> {
        let mut p = Parser {
            s: text.as_bytes(),
            text,
            i: 0,
            depth: 0,
        };
        p.ws();
        let v = p.value()?;
        p.ws();
        if p.i != p.s.len() {
            return Err(p.err("extra data"));
        }
        Ok(v)
    }

    /// From any serde value (for example a payload struct).
    pub fn from_serialize<T: serde::Serialize>(value: &T) -> Result<Self> {
        Ok(Self::from_value(serde_json::to_value(value)?))
    }

    fn from_value(v: serde_json::Value) -> Self {
        use serde_json::Value;
        match v {
            Value::Null => Self::Null,
            Value::Bool(b) => Self::Bool(b),
            Value::Number(n) => match (n.as_i64(), n.as_u64()) {
                (Some(i), _) => Self::Int(i.into()),
                (None, Some(u)) => Self::Int(u.into()),
                _ => Self::Float(n.as_f64().unwrap_or(f64::NAN)),
            },
            Value::String(s) => Self::Str(s),
            Value::Array(a) => Self::Array(a.into_iter().map(Self::from_value).collect()),
            Value::Object(o) => Self::Object(
                o.into_iter()
                    .map(|(k, v)| (k, Self::from_value(v)))
                    .collect(),
            ),
        }
    }

    pub fn get(&self, key: &str) -> Option<&PyJson> {
        match self {
            Self::Object(fields) => fields.iter().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }

    pub fn dumps(&self, opts: DumpOptions) -> Result<String> {
        let mut out = String::new();
        self.write(&mut out, opts, 0)?;
        Ok(out)
    }

    fn write(&self, out: &mut String, opts: DumpOptions, depth: usize) -> Result<()> {
        let (item_sep, key_sep) = match (opts.indent, opts.compact) {
            (Some(_), _) => (",", ": "),
            (None, true) => (",", ":"),
            (None, false) => (", ", ": "),
        };
        // Before each item, and before the closing bracket.
        let open = |out: &mut String| {
            if let Some(n) = opts.indent {
                out.push('\n');
                out.push_str(&" ".repeat(n * (depth + 1)));
            }
        };
        let close = |out: &mut String| {
            if let Some(n) = opts.indent {
                out.push('\n');
                out.push_str(&" ".repeat(n * depth));
            }
        };
        match self {
            Self::Null => out.push_str("null"),
            Self::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
            Self::Int(i) => write!(out, "{i}")?,
            Self::BigInt(digits) => out.push_str(digits),
            Self::Float(f) => out.push_str(&float_repr(*f)?),
            Self::Str(s) => write_str(out, s, opts.ensure_ascii),
            Self::Array(items) if items.is_empty() => out.push_str("[]"),
            Self::Array(items) => {
                out.push('[');
                for (i, item) in items.iter().enumerate() {
                    if i > 0 {
                        out.push_str(item_sep);
                    }
                    open(out);
                    item.write(out, opts, depth + 1)?;
                }
                close(out);
                out.push(']');
            }
            Self::Object(fields) if fields.is_empty() => out.push_str("{}"),
            Self::Object(fields) => {
                let mut fields: Vec<&(String, PyJson)> = fields.iter().collect();
                if opts.sort_keys {
                    // Python compares str by code point, as Rust does for &str.
                    fields.sort_by(|a, b| a.0.cmp(&b.0));
                }
                out.push('{');
                for (i, (k, v)) in fields.into_iter().enumerate() {
                    if i > 0 {
                        out.push_str(item_sep);
                    }
                    open(out);
                    write_str(out, k, opts.ensure_ascii);
                    out.push_str(key_sep);
                    v.write(out, opts, depth + 1)?;
                }
                close(out);
                out.push('}');
            }
        }
        Ok(())
    }
}

/// Python's `json` string escaping: `\"`, `\\`, `\n`, `\r`, `\t`, `\b`, `\f`,
/// other controls as `\u00XX`; with `ensure_ascii`, everything outside
/// `' '..='~'` (DEL included) as `\uXXXX`, UTF-16 surrogate pairs above
/// U+FFFF.
fn write_str(out: &mut String, s: &str, ensure_ascii: bool) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{08}' => out.push_str("\\b"),
            '\u{0c}' => out.push_str("\\f"),
            c if (c as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c if ensure_ascii && !(' '..='~').contains(&c) => {
                let mut buf = [0u16; 2];
                for unit in c.encode_utf16(&mut buf) {
                    let _ = write!(out, "\\u{unit:04x}");
                }
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

/// Python's `float.__repr__`: shortest round-trip digits, `.0` on integral
/// values, exponent form (`1e-05`, `1e+16`) below 1e-4 or from 1e16; and
/// `json.dumps`'s `NaN`, `Infinity`, `-Infinity` (its default `allow_nan`).
fn float_repr(f: f64) -> Result<String> {
    if f.is_nan() {
        return Ok("NaN".into());
    }
    if f.is_infinite() {
        return Ok(if f > 0.0 { "Infinity" } else { "-Infinity" }.into());
    }
    let abs = f.abs();
    if abs != 0.0 && !(1e-4..1e16).contains(&abs) {
        let e = format!("{f:e}"); // e.g. "1.5e-7", "1e16"
        let (mantissa, exp) = e.split_once('e').expect("exponent form");
        let exp: i32 = exp.parse()?;
        let sign = if exp < 0 { '-' } else { '+' };
        return Ok(format!("{mantissa}e{sign}{:02}", exp.abs()));
    }
    let s = format!("{f}");
    Ok(if s.contains('.') { s } else { format!("{s}.0") })
}

const MAX_DEPTH: usize = 512;

struct Parser<'a> {
    s: &'a [u8],
    text: &'a str,
    i: usize,
    depth: usize,
}

impl Parser<'_> {
    fn err(&self, what: &str) -> anyhow::Error {
        anyhow!("invalid JSON at byte {}: {what}", self.i)
    }

    fn ws(&mut self) {
        while matches!(self.s.get(self.i), Some(b' ' | b'\t' | b'\n' | b'\r')) {
            self.i += 1;
        }
    }

    fn eat(&mut self, lit: &str) -> bool {
        let found = self.s[self.i..].starts_with(lit.as_bytes());
        if found {
            self.i += lit.len();
        }
        found
    }

    fn value(&mut self) -> Result<PyJson> {
        match self.s.get(self.i) {
            Some(b'{') => self.nested(Self::object),
            Some(b'[') => self.nested(Self::array),
            Some(b'"') => Ok(PyJson::Str(self.string()?)),
            Some(b'-' | b'0'..=b'9') => {
                if self.eat("-Infinity") {
                    Ok(PyJson::Float(f64::NEG_INFINITY))
                } else {
                    self.number()
                }
            }
            _ if self.eat("null") => Ok(PyJson::Null),
            _ if self.eat("true") => Ok(PyJson::Bool(true)),
            _ if self.eat("false") => Ok(PyJson::Bool(false)),
            _ if self.eat("NaN") => Ok(PyJson::Float(f64::NAN)),
            _ if self.eat("Infinity") => Ok(PyJson::Float(f64::INFINITY)),
            _ => Err(self.err("expected a value")),
        }
    }

    fn nested(&mut self, f: fn(&mut Self) -> Result<PyJson>) -> Result<PyJson> {
        self.depth += 1;
        if self.depth > MAX_DEPTH {
            return Err(self.err("nested too deeply"));
        }
        let v = f(self);
        self.depth -= 1;
        v
    }

    fn array(&mut self) -> Result<PyJson> {
        self.i += 1;
        let mut items = Vec::new();
        self.ws();
        if self.eat("]") {
            return Ok(PyJson::Array(items));
        }
        loop {
            self.ws();
            items.push(self.value()?);
            self.ws();
            if self.eat("]") {
                return Ok(PyJson::Array(items));
            }
            if !self.eat(",") {
                return Err(self.err("expected `,` or `]`"));
            }
        }
    }

    fn object(&mut self) -> Result<PyJson> {
        self.i += 1;
        let mut fields: Vec<(String, PyJson)> = Vec::new();
        self.ws();
        if self.eat("}") {
            return Ok(PyJson::Object(fields));
        }
        loop {
            self.ws();
            if self.s.get(self.i) != Some(&b'"') {
                return Err(self.err("expected a string key"));
            }
            let k = self.string()?;
            self.ws();
            if !self.eat(":") {
                return Err(self.err("expected `:`"));
            }
            self.ws();
            let v = self.value()?;
            // json.loads keeps the last duplicate, in the first one's position.
            match fields.iter_mut().find(|(key, _)| *key == k) {
                Some(slot) => slot.1 = v,
                None => fields.push((k, v)),
            }
            self.ws();
            if self.eat("}") {
                return Ok(PyJson::Object(fields));
            }
            if !self.eat(",") {
                return Err(self.err("expected `,` or `}`"));
            }
        }
    }

    /// `-?(0|[1-9][0-9]*)(\.[0-9]+)?([eE][-+]?[0-9]+)?`, as in Python's scanner.
    fn number(&mut self) -> Result<PyJson> {
        let start = self.i;
        let digits = |p: &mut Self| {
            let from = p.i;
            while matches!(p.s.get(p.i), Some(b'0'..=b'9')) {
                p.i += 1;
            }
            p.i - from
        };
        self.eat("-");
        if self.eat("0") {
        } else if digits(self) == 0 {
            return Err(self.err("expected digits"));
        }
        let mut float = false;
        if self.s.get(self.i) == Some(&b'.') && matches!(self.s.get(self.i + 1), Some(b'0'..=b'9'))
        {
            self.i += 1;
            digits(self);
            float = true;
        }
        if matches!(self.s.get(self.i), Some(b'e' | b'E')) {
            let save = self.i;
            self.i += 1;
            if matches!(self.s.get(self.i), Some(b'+' | b'-')) {
                self.i += 1;
            }
            if digits(self) == 0 {
                self.i = save; // not an exponent; left for the caller to reject
            } else {
                float = true;
            }
        }
        let lit = &self.text[start..self.i];
        if float {
            return Ok(PyJson::Float(lit.parse()?));
        }
        Ok(match lit.parse::<i128>() {
            Ok(i) => PyJson::Int(i),
            Err(_) => PyJson::BigInt(lit.to_string()),
        })
    }

    fn hex4(&mut self) -> Result<u32> {
        let h = self
            .text
            .get(self.i..self.i + 4)
            .ok_or_else(|| self.err("short \\u escape"))?;
        let v = u32::from_str_radix(h, 16).map_err(|_| self.err("bad \\u escape"))?;
        self.i += 4;
        Ok(v)
    }

    fn string(&mut self) -> Result<String> {
        self.i += 1;
        let mut out = String::new();
        loop {
            let run = self.i;
            while matches!(self.s.get(self.i), Some(&b) if b != b'"' && b != b'\\' && b >= 0x20) {
                self.i += 1;
            }
            out.push_str(&self.text[run..self.i]);
            match self.s.get(self.i) {
                Some(b'"') => {
                    self.i += 1;
                    return Ok(out);
                }
                Some(b'\\') => {
                    self.i += 1;
                    let c = *self
                        .s
                        .get(self.i)
                        .ok_or_else(|| self.err("unterminated string"))?;
                    self.i += 1;
                    match c {
                        b'"' => out.push('"'),
                        b'\\' => out.push('\\'),
                        b'/' => out.push('/'),
                        b'b' => out.push('\u{08}'),
                        b'f' => out.push('\u{0c}'),
                        b'n' => out.push('\n'),
                        b'r' => out.push('\r'),
                        b't' => out.push('\t'),
                        b'u' => {
                            let hi = self.hex4()?;
                            let code = if (0xd800..0xdc00).contains(&hi) && self.eat("\\u") {
                                let lo = self.hex4()?;
                                if !(0xdc00..0xe000).contains(&lo) {
                                    return Err(self.err("unpaired surrogate"));
                                }
                                0x10000 + ((hi - 0xd800) << 10) + (lo - 0xdc00)
                            } else {
                                hi
                            };
                            out.push(
                                char::from_u32(code)
                                    .ok_or_else(|| self.err("unpaired surrogate"))?,
                            );
                        }
                        _ => return Err(self.err("invalid escape")),
                    }
                }
                Some(_) => return Err(self.err("control character in string")),
                None => return Err(self.err("unterminated string")),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Expected strings were produced by CPython 3.13's `json.dumps`.
    #[test]
    fn matches_python_signing_form() {
        let v = PyJson::parse(r#"{"b":1,"a":{"z":null,"y":[true,"é\n\u0001"]},"c":-2.5}"#).unwrap();
        assert_eq!(
            v.dumps(DumpOptions::SIGNING).unwrap(),
            "{\"a\":{\"y\":[true,\"é\\n\\u0001\"],\"z\":null},\"b\":1,\"c\":-2.5}"
        );
    }

    #[test]
    fn matches_python_default_form() {
        let v = PyJson::parse(r#"{"b":"é😀","a":[1.0,0.0001,0.00001,1e16,123.456,-0.0]}"#).unwrap();
        assert_eq!(
            v.dumps(DumpOptions::PYTHON_DEFAULT).unwrap(),
            r#"{"b": "\u00e9\ud83d\ude00", "a": [1.0, 0.0001, 1e-05, 1e+16, 123.456, -0.0]}"#
        );
    }

    #[test]
    fn matches_python_indented_form() {
        let v = PyJson::parse(r#"{"b":[1,{"c":[]},{}],"a":"é"}"#).unwrap();
        // json.dumps(v, indent=2, ensure_ascii=False)
        assert_eq!(
            v.dumps(DumpOptions::PRETTY).unwrap(),
            "{\n  \"b\": [\n    1,\n    {\n      \"c\": []\n    },\n    {}\n  ],\n  \"a\": \"é\"\n}"
        );
    }

    #[test]
    fn del_is_escaped_only_in_ascii_mode() {
        // json.dumps("a\x7fb") and json.dumps("a\x7fb", ensure_ascii=False)
        let v = PyJson::Str("a\u{7f}b".into());
        assert_eq!(
            v.dumps(DumpOptions::PYTHON_DEFAULT).unwrap(),
            r#""a\u007fb""#
        );
        assert_eq!(v.dumps(DumpOptions::SIGNING).unwrap(), "\"a\u{7f}b\"");
    }

    #[test]
    fn parses_like_json_loads() {
        let v =
            PyJson::parse(r#" {"a": [1, -0, 1.5e3, NaN, -Infinity, "\ud83d\ude00\/"]} "#).unwrap();
        // json.dumps(json.loads(...))
        assert_eq!(
            v.dumps(DumpOptions::PYTHON_DEFAULT).unwrap(),
            r#"{"a": [1, 0, 1500.0, NaN, -Infinity, "\ud83d\ude00/"]}"#
        );
        for bad in ["[1,]", "01", r#""\ud800""#, "\"a\u{1}\"", "{} x", "1."] {
            assert!(PyJson::parse(bad).is_err(), "{bad}");
        }
        assert!(PyJson::parse(&"[".repeat(600)).is_err(), "depth limit");
    }

    #[test]
    fn duplicate_keys_keep_the_last_value_in_the_first_position() {
        let v = PyJson::parse(r#"{"a":1,"b":2,"a":3}"#).unwrap();
        assert_eq!(
            v.dumps(DumpOptions::PYTHON_DEFAULT).unwrap(),
            r#"{"a": 3, "b": 2}"#
        );
    }
}
