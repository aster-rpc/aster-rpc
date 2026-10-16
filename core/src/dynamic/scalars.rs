//! The canonical `uuid`, `ulid` and `decimal` / `decimal(p, s)` scalars
//! (Aster-ContractIdentity.md, "Time, UUID, and Decimal"): string forms,
//! the `(p, s)` rule, and Fory DECIMAL's wire form.
//!
//! - `uuid` / `ulid`: 16 bytes as Fory BINARY; anything else is refused.
//!   JSON forms: hyphenated lowercase `8-4-4-4-12`, and 26 characters of
//!   Crockford base32.
//! - `decimal`: Fory DECIMAL (type id 40), `varint32` scale then the
//!   unscaled value (zig-zag `varuint64 << 1`, or a flagged little-endian
//!   magnitude). `decimal(p, s)` is the same wire at scale exactly `s`:
//!   encode and decode rescale by zero padding and refuse a value that would
//!   need rounding or has more than `p` digits.

use std::fmt;

use fory_core::buffer::{Reader, Writer};
use num_bigint::{BigInt, BigUint, Sign};

use super::value::{DynError, DynResult};

/// The largest precision a `decimal(p, s)` may declare.
pub const MAX_DECIMAL_PRECISION: u32 = 1000;
/// Fory 1.7.6's bounds on a DECIMAL (`serializer/decimal.rs`).
const MAX_DECIMAL_SCALE: i32 = 10_000;
const MAX_DECIMAL_MAGNITUDE_BYTES: usize = 10_000;

/// A decimal value: `unscaled × 10^-scale` (Fory `Decimal`'s shape).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct DynDecimal {
    pub unscaled: BigInt,
    pub scale: i32,
}

impl DynDecimal {
    pub fn new(unscaled: impl Into<BigInt>, scale: i32) -> Self {
        DynDecimal {
            unscaled: unscaled.into(),
            scale,
        }
    }

    /// Parse plain (`-12.5`, `+3`, `.25`) or exponent (`1.5E+3`, `2e-4`)
    /// notation. The scale is the number of digits after the point, less the
    /// exponent.
    pub fn parse(s: &str) -> Option<DynDecimal> {
        let s = s.trim();
        let (mantissa, exp) = match s.find(['e', 'E']) {
            Some(i) => (&s[..i], s[i + 1..].parse::<i64>().ok()?),
            None => (s, 0),
        };
        let (neg, body) = match mantissa.as_bytes().first() {
            Some(b'-') => (true, &mantissa[1..]),
            Some(b'+') => (false, &mantissa[1..]),
            _ => (false, mantissa),
        };
        let (int, frac) = body.split_once('.').unwrap_or((body, ""));
        if (int.is_empty() && frac.is_empty())
            || !int.bytes().chain(frac.bytes()).all(|c| c.is_ascii_digit())
        {
            return None;
        }
        let digits = format!("{int}{frac}");
        let magnitude: BigUint = if digits.is_empty() {
            BigUint::from(0u32)
        } else {
            digits.parse().ok()?
        };
        let scale = i64::try_from(frac.len()).ok()? - exp;
        let scale = i32::try_from(scale).ok()?;
        if !(-MAX_DECIMAL_SCALE..=MAX_DECIMAL_SCALE).contains(&scale) {
            return None;
        }
        let sign = if neg { Sign::Minus } else { Sign::Plus };
        Some(DynDecimal::new(
            BigInt::from_biguint(sign, magnitude),
            scale,
        ))
    }

    /// Rescale to exactly `scale` digits after the point and check at most
    /// `precision` digits: the `decimal(p, s)` rule. Zero padding is exact;
    /// a value whose dropped digits are not all zero, or with too many
    /// digits, is refused (the error is the reason).
    pub fn rescale(&self, precision: u32, scale: u32) -> Result<DynDecimal, String> {
        let target = i64::from(scale);
        let have = i64::from(self.scale);
        let unscaled = if have > target {
            let factor = BigInt::from(pow10((have - target) as u32));
            if &self.unscaled % &factor != BigInt::from(0) {
                return Err(format!(
                    "decimal({precision},{scale}): {self} needs rounding to {scale} decimal places"
                ));
            }
            &self.unscaled / &factor
        } else {
            let pad = u32::try_from(target - have)
                .map_err(|_| format!("decimal({precision},{scale}): {self} is out of range"))?;
            &self.unscaled * BigInt::from(pow10(pad))
        };
        if unscaled.magnitude() >= &pow10(precision) {
            return Err(format!(
                "decimal({precision},{scale}): {self} has more than {precision} digits"
            ));
        }
        Ok(DynDecimal::new(unscaled, scale as i32))
    }

    /// Fory DECIMAL data: `varint32` scale, then the unscaled value.
    pub(crate) fn write(&self, w: &mut Writer) -> DynResult<()> {
        if !(-MAX_DECIMAL_SCALE..=MAX_DECIMAL_SCALE).contains(&self.scale) {
            return Err(DynError::invalid(format!(
                "decimal scale {} is outside [-{MAX_DECIMAL_SCALE}, {MAX_DECIMAL_SCALE}]",
                self.scale
            )));
        }
        if self.unscaled.bits() > (MAX_DECIMAL_MAGNITUDE_BYTES as u64) * 8 {
            return Err(DynError::invalid(format!(
                "decimal magnitude exceeds {MAX_DECIMAL_MAGNITUDE_BYTES} bytes"
            )));
        }
        w.write_var_i32(self.scale);
        if let Some(small) = small_encoding(&self.unscaled) {
            w.write_var_u64(zigzag(small) << 1);
            return Ok(());
        }
        let (sign, magnitude) = self.unscaled.to_bytes_le();
        let meta = ((magnitude.len() as u64) << 1) | u64::from(sign == Sign::Minus);
        w.write_var_u64((meta << 1) | 1);
        w.write_bytes(&magnitude);
        Ok(())
    }

    /// Read Fory DECIMAL data, with Fory's canonical-form checks.
    pub(crate) fn read(r: &mut Reader) -> DynResult<DynDecimal> {
        let scale = r.read_var_i32()?;
        if !(-MAX_DECIMAL_SCALE..=MAX_DECIMAL_SCALE).contains(&scale) {
            return Err(DynError::decode(format!(
                "decimal scale {scale} is outside [-{MAX_DECIMAL_SCALE}, {MAX_DECIMAL_SCALE}]"
            )));
        }
        let header = r.read_var_u64()?;
        if header & 1 == 0 {
            return Ok(DynDecimal::new(unzigzag(header >> 1), scale));
        }
        let meta = header >> 1;
        let negative = meta & 1 != 0;
        let len = meta >> 1;
        if len == 0 || len > MAX_DECIMAL_MAGNITUDE_BYTES as u64 {
            return Err(DynError::decode(format!(
                "decimal magnitude length {len} is outside 1..={MAX_DECIMAL_MAGNITUDE_BYTES}"
            )));
        }
        let bytes = r.read_bytes(len as usize)?;
        if bytes[bytes.len() - 1] == 0 {
            return Err(DynError::decode(
                "non-canonical decimal magnitude: trailing zero byte",
            ));
        }
        let magnitude = BigInt::from_bytes_le(Sign::Plus, bytes);
        Ok(DynDecimal::new(
            if negative { -magnitude } else { magnitude },
            scale,
        ))
    }
}

/// Plain notation: `-12.3400`, `1500` (a negative scale appends zeros).
impl fmt::Display for DynDecimal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let sign = if self.unscaled.sign() == Sign::Minus {
            "-"
        } else {
            ""
        };
        let digits = self.unscaled.magnitude().to_string();
        if self.scale <= 0 {
            let zeros = if self.unscaled.sign() == Sign::NoSign {
                0
            } else {
                self.scale.unsigned_abs() as usize
            };
            return write!(f, "{sign}{digits}{}", "0".repeat(zeros));
        }
        let scale = self.scale as usize;
        let digits = format!("{digits:0>width$}", width = scale + 1);
        let (int, frac) = digits.split_at(digits.len() - scale);
        write!(f, "{sign}{int}.{frac}")
    }
}

fn pow10(n: u32) -> BigUint {
    BigUint::from(10u32).pow(n)
}

fn small_encoding(v: &BigInt) -> Option<i64> {
    let small = i64::try_from(v).ok()?;
    (zigzag(small) & (1u64 << 63) == 0).then_some(small)
}

fn zigzag(v: i64) -> u64 {
    ((v << 1) ^ (v >> 63)) as u64
}

fn unzigzag(v: u64) -> i64 {
    ((v >> 1) as i64) ^ -((v & 1) as i64)
}

/// Parse `decimal(<p>,<s>)` exactly as the spec writes it: ASCII, base-10,
/// no leading zeros, no spaces, `1 <= p <= 1000`, `0 <= s <= p`.
pub(crate) fn parse_decimal_params(name: &str) -> Option<Result<(u32, u32), String>> {
    let inner = name.strip_prefix("decimal(")?.strip_suffix(')')?;
    let bad = || {
        Err(format!(
            "{name:?} is not decimal(p,s) with 1 <= p <= 1000, 0 <= s <= p"
        ))
    };
    let Some((p, s)) = inner.split_once(',') else {
        return Some(bad());
    };
    let number = |t: &str| -> Option<u32> {
        let canonical = !t.is_empty()
            && t.bytes().all(|c| c.is_ascii_digit())
            && (t == "0" || !t.starts_with('0'));
        canonical.then(|| t.parse().ok()).flatten()
    };
    match (number(p), number(s)) {
        (Some(p), Some(s)) if (1..=MAX_DECIMAL_PRECISION).contains(&p) && s <= p => {
            Some(Ok((p, s)))
        }
        _ => Some(bad()),
    }
}

// ── 16-byte identifiers ──────────────────────────────────────────────────────

/// Hyphenated lowercase `8-4-4-4-12`.
pub fn format_uuid(b: &[u8; 16]) -> String {
    let h = hex::encode(b);
    format!(
        "{}-{}-{}-{}-{}",
        &h[0..8],
        &h[8..12],
        &h[12..16],
        &h[16..20],
        &h[20..32]
    )
}

/// The hyphenated form (any case) or 32 hex digits.
pub fn parse_uuid(s: &str) -> Option<[u8; 16]> {
    let s = s.trim();
    let digits: String = match s.len() {
        36 => {
            let b = s.as_bytes();
            if [8, 13, 18, 23].iter().any(|&i| b[i] != b'-') {
                return None;
            }
            s.chars().filter(|&c| c != '-').collect()
        }
        32 => s.to_string(),
        _ => return None,
    };
    let mut out = [0u8; 16];
    hex::decode_to_slice(&digits, &mut out).ok()?;
    Some(out)
}

const CROCKFORD: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";

/// 26 characters of Crockford base32, upper case.
pub fn format_ulid(b: &[u8; 16]) -> String {
    let n = u128::from_be_bytes(*b);
    (0..26)
        .map(|i| CROCKFORD[((n >> (125 - 5 * i)) & 31) as usize] as char)
        .collect()
}

/// 26 Crockford base32 characters, case-insensitive, first `0`-`7`.
pub fn parse_ulid(s: &str) -> Option<[u8; 16]> {
    let s = s.trim();
    if s.len() != 26 {
        return None;
    }
    let mut n: u128 = 0;
    for (i, c) in s.bytes().enumerate() {
        let c = c.to_ascii_uppercase();
        let v = CROCKFORD.iter().position(|&a| a == c)? as u128;
        if i == 0 && v > 7 {
            return None;
        }
        n = (n << 5) | v;
    }
    Some(n.to_be_bytes())
}

/// 16 bytes, or why not (`what` is `uuid` / `ulid`).
pub(crate) fn id16(bytes: &[u8], what: &str) -> Result<[u8; 16], String> {
    <[u8; 16]>::try_from(bytes)
        .map_err(|_| format!("{what}: expected 16 bytes, got {}", bytes.len()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decimal_parse_format_rescale() {
        let d = DynDecimal::parse("-12345.67").unwrap();
        assert_eq!((d.unscaled.clone(), d.scale), (BigInt::from(-1_234_567), 2));
        assert_eq!(d.rescale(18, 4).unwrap().to_string(), "-12345.6700");
        assert!(DynDecimal::parse("1.23456")
            .unwrap()
            .rescale(18, 4)
            .is_err());
        assert!(DynDecimal::parse("123456").unwrap().rescale(5, 0).is_err());
        assert_eq!(
            DynDecimal::parse("7.0")
                .unwrap()
                .rescale(5, 0)
                .unwrap()
                .to_string(),
            "7"
        );
        assert_eq!(DynDecimal::parse("1.5E+3").unwrap().to_string(), "1500");
        assert_eq!(DynDecimal::parse("2e-4").unwrap().to_string(), "0.0002");
        assert_eq!(DynDecimal::parse(".25").unwrap().to_string(), "0.25");
        for bad in ["", ".", "1.2.3", "abc", "1e", "--1"] {
            assert!(DynDecimal::parse(bad).is_none(), "{bad}");
        }
    }

    #[test]
    fn decimal_params_are_strict() {
        assert_eq!(parse_decimal_params("decimal(18,4)"), Some(Ok((18, 4))));
        assert_eq!(parse_decimal_params("decimal(5,0)"), Some(Ok((5, 0))));
        assert_eq!(parse_decimal_params("decimal"), None);
        for bad in [
            "decimal(18, 4)",
            "decimal(018,4)",
            "decimal(0,0)",
            "decimal(4,5)",
            "decimal(1001,0)",
            "decimal(18)",
        ] {
            assert!(matches!(parse_decimal_params(bad), Some(Err(_))), "{bad}");
        }
    }

    #[test]
    fn decimal_wire_round_trips_small_and_big() {
        for s in ["0", "-1", "12345.6789", "12345678901234567890.1234567890"] {
            let d = DynDecimal::parse(s).unwrap();
            let mut buf = Vec::new();
            d.write(&mut Writer::from_buffer(&mut buf)).unwrap();
            let back = DynDecimal::read(&mut Reader::new(&buf)).unwrap();
            assert_eq!(back, d, "{s}");
        }
    }

    #[test]
    fn identifier_string_forms() {
        let u = parse_uuid("0190F2A8-1B2C-7D3E-8F40-123456789ABC").unwrap();
        assert_eq!(format_uuid(&u), "0190f2a8-1b2c-7d3e-8f40-123456789abc");
        assert_eq!(parse_uuid("0190f2a81b2c7d3e8f40123456789abc"), Some(u));
        assert!(parse_uuid("0190f2a8_1b2c-7d3e-8f40-123456789abc").is_none());
        let l = parse_ulid("01arz3ndektsv4rrffq69g5fav").unwrap();
        assert_eq!(format_ulid(&l), "01ARZ3NDEKTSV4RRFFQ69G5FAV");
        assert!(parse_ulid("81ARZ3NDEKTSV4RRFFQ69G5FAV").is_none());
        assert!(id16(&[0; 15], "uuid").is_err());
    }
}
