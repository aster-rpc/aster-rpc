//! The little CBOR (RFC 8949) WebAuthn needs: definite-length unsigned and
//! negative integers, byte and text strings, arrays, maps and the simple
//! values `false`, `true` and `null`. Nothing else is accepted, and nesting
//! is bounded, so a hostile attestation object fails fast.

/// A decoded CBOR item.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Value {
    Uint(u64),
    /// A negative integer, `-1 - n` for the encoded `n`.
    Neg(i128),
    Bytes(Vec<u8>),
    Text(String),
    Array(Vec<Value>),
    Map(Vec<(Value, Value)>),
    Bool(bool),
    Null,
}

impl Value {
    /// The integer value of `Uint` or `Neg`.
    pub fn as_int(&self) -> Option<i128> {
        match self {
            Self::Uint(n) => Some(i128::from(*n)),
            Self::Neg(n) => Some(*n),
            _ => None,
        }
    }

    pub fn as_bytes(&self) -> Option<&[u8]> {
        match self {
            Self::Bytes(bytes) => Some(bytes),
            _ => None,
        }
    }

    pub fn as_text(&self) -> Option<&str> {
        match self {
            Self::Text(text) => Some(text),
            _ => None,
        }
    }

    /// The value under integer key `key` of a map.
    pub fn get_int(&self, key: i128) -> Option<&Value> {
        let Self::Map(entries) = self else {
            return None;
        };
        entries
            .iter()
            .find(|(k, _)| k.as_int() == Some(key))
            .map(|(_, v)| v)
    }

    /// The value under text key `key` of a map.
    pub fn get_text(&self, key: &str) -> Option<&Value> {
        let Self::Map(entries) = self else {
            return None;
        };
        entries
            .iter()
            .find(|(k, _)| k.as_text() == Some(key))
            .map(|(_, v)| v)
    }
}

/// Why CBOR did not decode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Malformed;

const MAX_DEPTH: usize = 8;
const MAX_ITEMS: u64 = 256;

/// Decode one item from the start of `input`; return it and the bytes it
/// used.
pub fn decode(input: &[u8]) -> Result<(Value, usize), Malformed> {
    let mut reader = Reader { input, at: 0 };
    let value = reader.item(0)?;
    Ok((value, reader.at))
}

/// Decode exactly one item spanning all of `input`.
pub fn decode_all(input: &[u8]) -> Result<Value, Malformed> {
    let (value, used) = decode(input)?;
    if used != input.len() {
        return Err(Malformed);
    }
    Ok(value)
}

struct Reader<'a> {
    input: &'a [u8],
    at: usize,
}

impl Reader<'_> {
    fn take(&mut self, n: usize) -> Result<&[u8], Malformed> {
        let end = self.at.checked_add(n).ok_or(Malformed)?;
        let bytes = self.input.get(self.at..end).ok_or(Malformed)?;
        self.at = end;
        Ok(bytes)
    }

    fn argument(&mut self, info: u8) -> Result<u64, Malformed> {
        Ok(match info {
            0..=23 => u64::from(info),
            24 => u64::from(self.take(1)?[0]),
            25 => u64::from(u16::from_be_bytes(self.take(2)?.try_into().unwrap())),
            26 => u64::from(u32::from_be_bytes(self.take(4)?.try_into().unwrap())),
            27 => u64::from_be_bytes(self.take(8)?.try_into().unwrap()),
            // 28..=30 reserved; 31 is indefinite length, not accepted.
            _ => return Err(Malformed),
        })
    }

    fn length(&mut self, info: u8) -> Result<usize, Malformed> {
        let n = self.argument(info)?;
        let n = usize::try_from(n).map_err(|_| Malformed)?;
        // Every item takes at least a byte: a length past the input is a lie.
        if n > self.input.len() - self.at {
            return Err(Malformed);
        }
        Ok(n)
    }

    fn item(&mut self, depth: usize) -> Result<Value, Malformed> {
        if depth > MAX_DEPTH {
            return Err(Malformed);
        }
        let initial = self.take(1)?[0];
        let (major, info) = (initial >> 5, initial & 0x1f);
        Ok(match major {
            0 => Value::Uint(self.argument(info)?),
            1 => Value::Neg(-1 - i128::from(self.argument(info)?)),
            2 => {
                let n = self.length(info)?;
                Value::Bytes(self.take(n)?.to_vec())
            }
            3 => {
                let n = self.length(info)?;
                let text = std::str::from_utf8(self.take(n)?).map_err(|_| Malformed)?;
                Value::Text(text.to_string())
            }
            4 => {
                let n = self.length(info)?;
                if n as u64 > MAX_ITEMS {
                    return Err(Malformed);
                }
                let mut items = Vec::with_capacity(n);
                for _ in 0..n {
                    items.push(self.item(depth + 1)?);
                }
                Value::Array(items)
            }
            5 => {
                let n = self.length(info)?;
                if n as u64 > MAX_ITEMS {
                    return Err(Malformed);
                }
                let mut entries = Vec::with_capacity(n);
                for _ in 0..n {
                    let key = self.item(depth + 1)?;
                    let value = self.item(depth + 1)?;
                    if entries.iter().any(|(k, _)| *k == key) {
                        return Err(Malformed);
                    }
                    entries.push((key, value));
                }
                Value::Map(entries)
            }
            7 => match info {
                20 => Value::Bool(false),
                21 => Value::Bool(true),
                22 => Value::Null,
                _ => return Err(Malformed),
            },
            // Tags (6) are not used by WebAuthn.
            _ => return Err(Malformed),
        })
    }
}

/// Encode `value` (definite lengths, shortest arguments). For the software
/// authenticator and tests.
pub fn encode(value: &Value) -> Vec<u8> {
    let mut out = Vec::new();
    write(&mut out, value);
    out
}

fn head(out: &mut Vec<u8>, major: u8, n: u64) {
    let major = major << 5;
    match n {
        0..=23 => out.push(major | n as u8),
        24..=0xff => out.extend([major | 24, n as u8]),
        0x100..=0xffff => {
            out.push(major | 25);
            out.extend((n as u16).to_be_bytes());
        }
        0x1_0000..=0xffff_ffff => {
            out.push(major | 26);
            out.extend((n as u32).to_be_bytes());
        }
        _ => {
            out.push(major | 27);
            out.extend(n.to_be_bytes());
        }
    }
}

fn write(out: &mut Vec<u8>, value: &Value) {
    match value {
        Value::Uint(n) => head(out, 0, *n),
        Value::Neg(n) => head(out, 1, (-1 - *n) as u64),
        Value::Bytes(bytes) => {
            head(out, 2, bytes.len() as u64);
            out.extend(bytes);
        }
        Value::Text(text) => {
            head(out, 3, text.len() as u64);
            out.extend(text.as_bytes());
        }
        Value::Array(items) => {
            head(out, 4, items.len() as u64);
            for item in items {
                write(out, item);
            }
        }
        Value::Map(entries) => {
            head(out, 5, entries.len() as u64);
            for (key, value) in entries {
                write(out, key);
                write(out, value);
            }
        }
        Value::Bool(false) => out.push(0xf4),
        Value::Bool(true) => out.push(0xf5),
        Value::Null => out.push(0xf6),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rfc_8949_appendix_a_examples() {
        let cases: &[(&[u8], Value)] = &[
            (&[0x00], Value::Uint(0)),
            (&[0x17], Value::Uint(23)),
            (&[0x18, 0x18], Value::Uint(24)),
            (&[0x19, 0x03, 0xe8], Value::Uint(1000)),
            (&[0x20], Value::Neg(-1)),
            (&[0x26], Value::Neg(-7)),
            (&[0x38, 0x63], Value::Neg(-100)),
            (&[0x43, 1, 2, 3], Value::Bytes(vec![1, 2, 3])),
            (&[0x64, b'I', b'E', b'T', b'F'], Value::Text("IETF".into())),
            (
                &[0x82, 0x01, 0x02],
                Value::Array(vec![Value::Uint(1), Value::Uint(2)]),
            ),
            (
                &[0xa1, 0x61, b'a', 0x01],
                Value::Map(vec![(Value::Text("a".into()), Value::Uint(1))]),
            ),
            (&[0xf4], Value::Bool(false)),
            (&[0xf6], Value::Null),
        ];
        for (bytes, value) in cases {
            assert_eq!(decode_all(bytes).as_ref(), Ok(value), "{bytes:02x?}");
            assert_eq!(&encode(value), bytes);
        }
    }

    #[test]
    fn hostile_input_is_refused() {
        for bytes in [
            &[][..],
            &[0x5f, 0x41, 0x00, 0xff],       // indefinite byte string
            &[0x5a, 0xff, 0xff, 0xff, 0xff], // length past the input
            &[0xc1, 0x00],                   // a tag
            &[0xa2, 0x01, 0x00, 0x01, 0x00], // duplicate map key
            &[0x81; 12],                     // nested too deep
            &[0x62, 0xff, 0xfe],             // text that is not UTF-8
            &[0x00, 0x00],                   // trailing bytes
        ] {
            assert_eq!(decode_all(bytes), Err(Malformed), "{bytes:02x?}");
        }
    }
}
