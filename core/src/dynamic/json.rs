//! JSON ↔ [`DynValue`].
//!
//! Output follows the Aster JSON encoding table (Aster-ContractIdentity.md,
//! "JSON Mode Encoding Table"): 64-bit integers as decimal strings, `binary`
//! as unpadded base64url, enums by name, unions as
//! `{"_type": "<payload type>", ...payload fields}`, maps with string keys as
//! objects and other maps as `[[key, value], ...]`, timestamps as ISO-8601 UTC
//! with six fractional digits.
//!
//! Input is schema-free ([`DynValue::from_json`]): integers become `I64`/`U64`,
//! other numbers `F64`, objects `Map`s with string keys. Pair it with
//! [`DynSchema::coerce`](super::DynSchema::coerce) (the encoder does this) to
//! reach the exact typed shape — that is where 64-bit strings, base64 and enum
//! names are interpreted against the target type.

use base64::engine::general_purpose::{STANDARD, STANDARD_NO_PAD, URL_SAFE, URL_SAFE_NO_PAD};
use base64::Engine as _;
use serde_json::{Map, Number, Value};

use super::value::DynValue;

/// How [`DynValue::to_json`] renders values the JSON table leaves a choice on.
#[derive(Clone, Debug)]
pub struct JsonOptions {
    /// Render `int64`/`uint64` values as decimal strings (the Aster JSON
    /// rule). `false` emits plain numbers (lossy above 2^53 in JavaScript).
    pub int64_as_string: bool,
    /// Render `binary` as base64url without padding (the Aster JSON rule);
    /// `false` uses standard padded base64.
    pub base64url: bool,
}

impl Default for JsonOptions {
    fn default() -> Self {
        JsonOptions {
            int64_as_string: true,
            base64url: true,
        }
    }
}

impl DynValue {
    /// A schema-free value from JSON: `null`, `bool`, `I64` (or `U64` above
    /// `i64::MAX`), `F64`, `String`, `List`, and `Map` (string keys, in
    /// document order).
    pub fn from_json(v: &Value) -> DynValue {
        match v {
            Value::Null => DynValue::Null,
            Value::Bool(b) => DynValue::Bool(*b),
            Value::Number(n) => {
                if let Some(i) = n.as_i64() {
                    DynValue::I64(i)
                } else if let Some(u) = n.as_u64() {
                    DynValue::U64(u)
                } else {
                    DynValue::F64(n.as_f64().unwrap_or(f64::NAN))
                }
            }
            Value::String(s) => DynValue::String(s.clone()),
            Value::Array(a) => DynValue::List(a.iter().map(DynValue::from_json).collect()),
            Value::Object(o) => DynValue::Map(
                o.iter()
                    .map(|(k, v)| (DynValue::String(k.clone()), DynValue::from_json(v)))
                    .collect(),
            ),
        }
    }

    /// Render as JSON per the Aster JSON table (see [`JsonOptions`]).
    pub fn to_json(&self, opts: &JsonOptions) -> Value {
        match self {
            DynValue::Null => Value::Null,
            DynValue::Bool(b) => Value::Bool(*b),
            DynValue::I8(x) => Value::from(*x),
            DynValue::I16(x) => Value::from(*x),
            DynValue::I32(x) => Value::from(*x),
            DynValue::U8(x) => Value::from(*x),
            DynValue::U16(x) => Value::from(*x),
            DynValue::U32(x) => Value::from(*x),
            DynValue::I64(x) => {
                if opts.int64_as_string {
                    Value::String(x.to_string())
                } else {
                    Value::from(*x)
                }
            }
            DynValue::U64(x) => {
                if opts.int64_as_string {
                    Value::String(x.to_string())
                } else {
                    Value::from(*x)
                }
            }
            // The shortest decimal that reads back as the same f32 (`0.1`, not
            // `0.10000000149011612`).
            DynValue::F32(f) => float_json(f.to_string().parse::<f64>().unwrap_or(*f as f64)),
            DynValue::F64(f) => float_json(*f),
            DynValue::String(s) => Value::String(s.clone()),
            DynValue::Bytes(b) => Value::String(if opts.base64url {
                URL_SAFE_NO_PAD.encode(b)
            } else {
                STANDARD.encode(b)
            }),
            DynValue::Timestamp { seconds, nanos } => {
                Value::String(format_timestamp(*seconds, *nanos))
            }
            DynValue::Date { days } => Value::String(format_date(*days)),
            DynValue::Duration { seconds, nanos } => {
                let mut m = Map::new();
                m.insert("seconds".into(), Value::String(seconds.to_string()));
                m.insert("nanos".into(), Value::from(*nanos));
                Value::Object(m)
            }
            DynValue::List(items) | DynValue::Set(items) => {
                Value::Array(items.iter().map(|v| v.to_json(opts)).collect())
            }
            DynValue::Map(entries) => {
                if entries
                    .iter()
                    .all(|(k, _)| matches!(k, DynValue::String(_)))
                {
                    let mut m = Map::new();
                    for (k, v) in entries {
                        if let DynValue::String(k) = k {
                            m.insert(k.clone(), v.to_json(opts));
                        }
                    }
                    Value::Object(m)
                } else {
                    Value::Array(
                        entries
                            .iter()
                            .map(|(k, v)| Value::Array(vec![k.to_json(opts), v.to_json(opts)]))
                            .collect(),
                    )
                }
            }
            DynValue::Struct(s) => {
                let mut m = Map::new();
                for (k, v) in &s.fields {
                    m.insert(k.clone(), v.to_json(opts));
                }
                Value::Object(m)
            }
            DynValue::Enum(e) => match &e.name {
                Some(n) => Value::String(n.clone()),
                None => Value::from(e.ordinal),
            },
            DynValue::Union(u) => {
                let tag = match (&*u.value, &u.case_name) {
                    (DynValue::Struct(s), _) if !s.type_name.is_empty() => s.type_name.clone(),
                    (_, Some(n)) => n.clone(),
                    _ => u.case_id.to_string(),
                };
                let mut m = Map::new();
                m.insert("_type".into(), Value::String(tag));
                match u.value.to_json(opts) {
                    Value::Object(fields) => {
                        for (k, v) in fields {
                            m.insert(k, v);
                        }
                    }
                    other => {
                        m.insert("value".into(), other);
                    }
                }
                Value::Object(m)
            }
        }
    }
}

fn float_json(f: f64) -> Value {
    match Number::from_f64(f) {
        Some(n) => Value::Number(n),
        None if f.is_nan() => Value::String("NaN".into()),
        None if f > 0.0 => Value::String("Infinity".into()),
        None => Value::String("-Infinity".into()),
    }
}

/// Base64 in any of the four common spellings.
pub(crate) fn decode_base64(s: &str) -> Option<Vec<u8>> {
    let s = s.trim();
    URL_SAFE_NO_PAD
        .decode(s)
        .or_else(|_| URL_SAFE.decode(s))
        .or_else(|_| STANDARD.decode(s))
        .or_else(|_| STANDARD_NO_PAD.decode(s))
        .ok()
}

// ── calendar (proleptic Gregorian, UTC) ──────────────────────────────────────

fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m, d)
}

pub(crate) fn format_date(days: i64) -> String {
    let (y, m, d) = civil_from_days(days);
    format!("{y:04}-{m:02}-{d:02}")
}

/// `YYYY-MM-DDTHH:MM:SS.ffffffZ` (microseconds, truncated).
pub(crate) fn format_timestamp(seconds: i64, nanos: u32) -> String {
    let days = seconds.div_euclid(86_400);
    let sod = seconds.rem_euclid(86_400);
    format!(
        "{}T{:02}:{:02}:{:02}.{:06}Z",
        format_date(days),
        sod / 3600,
        (sod % 3600) / 60,
        sod % 60,
        nanos / 1000
    )
}

fn parse_ymd(s: &str) -> Option<i64> {
    let mut it = s.splitn(3, '-');
    let y: i64 = it.next()?.parse().ok()?;
    let m: i64 = it.next()?.parse().ok()?;
    let d: i64 = it.next()?.parse().ok()?;
    if !(1..=12).contains(&m) || !(1..=31).contains(&d) {
        return None;
    }
    Some(days_from_civil(y, m, d))
}

pub(crate) fn parse_date(s: &str) -> Option<i64> {
    let s = s.trim();
    if s.len() != 10 {
        return None;
    }
    parse_ymd(s)
}

/// ISO-8601 / RFC 3339: `YYYY-MM-DDTHH:MM:SS[.fraction](Z|±HH:MM)`.
pub(crate) fn parse_timestamp(s: &str) -> Option<(i64, u32)> {
    let s = s.trim();
    let (date, rest) = s.split_at_checked(10)?;
    let days = parse_ymd(date)?;
    let rest = rest.strip_prefix(['T', 't', ' '])?;
    let (hms, mut tail) = rest.split_at_checked(8)?;
    let mut parts = hms.split(':');
    let h: i64 = parts.next()?.parse().ok()?;
    let mi: i64 = parts.next()?.parse().ok()?;
    let se: i64 = parts.next()?.parse().ok()?;
    if h > 23 || mi > 59 || se > 60 {
        return None;
    }
    let mut nanos: u32 = 0;
    if let Some(frac) = tail.strip_prefix('.') {
        let digits = frac.chars().take_while(|c| c.is_ascii_digit()).count();
        if digits == 0 {
            return None;
        }
        let mut scaled = frac[..digits.min(9)].to_string();
        while scaled.len() < 9 {
            scaled.push('0');
        }
        nanos = scaled.parse().ok()?;
        tail = &frac[digits..];
    }
    let offset = match tail {
        "Z" | "z" => 0,
        t if (t.starts_with('+') || t.starts_with('-')) && t.len() == 6 => {
            let sign = if t.starts_with('-') { -1 } else { 1 };
            let oh: i64 = t[1..3].parse().ok()?;
            let om: i64 = t[4..6].parse().ok()?;
            sign * (oh * 3600 + om * 60)
        }
        _ => return None,
    };
    Some((days * 86_400 + h * 3600 + mi * 60 + se - offset, nanos))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn calendar_round_trips() {
        for days in [-719_468, -1, 0, 1, 18_000, 20_000, 2_932_896] {
            assert_eq!(parse_date(&format_date(days)), Some(days), "{days}");
        }
        assert_eq!(format_date(0), "1970-01-01");
        assert_eq!(
            format_timestamp(1_700_000_000, 123_456_789),
            "2023-11-14T22:13:20.123456Z"
        );
        assert_eq!(
            parse_timestamp("2023-11-14T22:13:20.123456789Z"),
            Some((1_700_000_000, 123_456_789))
        );
        assert_eq!(
            parse_timestamp("2023-11-14T23:13:20+01:00"),
            Some((1_700_000_000, 0))
        );
        assert_eq!(parse_timestamp("2023-11-14"), None);
    }

    #[test]
    fn base64_spellings() {
        for s in ["AQID_w", "AQID_w==", "AQID/w==", "AQID/w"] {
            assert_eq!(decode_base64(s), Some(vec![1, 2, 3, 255]), "{s}");
        }
    }
}
