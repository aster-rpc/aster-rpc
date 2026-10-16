//! Canonical identifier and fixed-point payload types: `uuid`, `ulid` and
//! `decimal(p, s)` (Aster-ContractIdentity.md, "Time, UUID, and Decimal").
//!
//! - `uuid` and `ulid` are 16 bytes, written as Fory `BINARY` (type id 41) of
//!   length 16, and are distinct contract types. A payload field holds them
//!   as `[u8; 16]` marked `#[aster(uuid)]` / `#[aster(ulid)]`: Fory 1.7's
//!   derive serializes a BINARY field only from `Vec<u8>` / `[u8; N]` syntax
//!   (it refuses a custom serializer whose type is BINARY), and its
//!   `[u8; 16]` decoder already refuses any other length. [`Uuid`] and
//!   [`Ulid`] convert to and from the array and parse / format the string
//!   forms.
//! - [`FixedDecimal<P, S>`] is a decimal constrained to precision `P` (total
//!   digits) and scale `S` (digits after the point), published as
//!   `decimal(P,S)`. It is written as Fory `DECIMAL` (type id 40) with scale
//!   exactly `S`. Construction rescales by zero padding and refuses a value
//!   that would need rounding or has more than `P` digits; a reader applies
//!   the same rule to what arrives. The unconstrained `decimal` is
//!   [`Decimal`] itself.
//!
//! `FixedDecimal` is a newtype with its own Fory `Serializer` rather than a
//! field attribute on [`Decimal`]: Fory's derive serializes a field by its
//! Rust type, so a type is the only place the `(P, S)` check can run on both
//! the write and the read path.

use std::fmt;
use std::str::FromStr;

use fory_core::{Error, ReadContext, Serializer, TypeId, WriteContext};
use num_bigint::{BigInt, BigUint, Sign};

use super::contract::{Decimal, Leaf, WireField};

/// A value that does not satisfy a canonical scalar type's rule: a malformed
/// UUID / ULID string, a byte string that is not 16 bytes, or a decimal that
/// does not fit its declared precision and scale.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScalarError(String);

impl fmt::Display for ScalarError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ScalarError {}

fn read_type_id(context: &mut ReadContext, expected: TypeId) -> Result<(), Error> {
    let remote = context.reader.read_u8()? as u32;
    if remote != expected as u32 {
        return Err(Error::type_mismatch(expected as u32, remote));
    }
    Ok(())
}

// ── 16-byte identifiers ──────────────────────────────────────────────────────

macro_rules! id_type {
    ($ty:ident, $what:literal) => {
        impl $ty {
            /// The identifier with these 16 bytes.
            pub const fn from_bytes(bytes: [u8; 16]) -> Self {
                $ty(bytes)
            }

            /// The 16 bytes on the wire.
            pub const fn as_bytes(&self) -> &[u8; 16] {
                &self.0
            }

            /// The 16 bytes as a big-endian integer.
            pub const fn to_u128(&self) -> u128 {
                u128::from_be_bytes(self.0)
            }

            /// The identifier whose bytes are `n`, big-endian.
            pub const fn from_u128(n: u128) -> Self {
                $ty(n.to_be_bytes())
            }
        }

        impl From<[u8; 16]> for $ty {
            fn from(bytes: [u8; 16]) -> Self {
                $ty(bytes)
            }
        }

        impl From<$ty> for [u8; 16] {
            fn from(id: $ty) -> Self {
                id.0
            }
        }

        impl TryFrom<&[u8]> for $ty {
            type Error = ScalarError;

            fn try_from(bytes: &[u8]) -> Result<Self, ScalarError> {
                <[u8; 16]>::try_from(bytes).map($ty).map_err(|_| {
                    ScalarError(format!("{}: expected 16 bytes, got {}", $what, bytes.len()))
                })
            }
        }

        impl fmt::Debug for $ty {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "{}({})", stringify!($ty), self)
            }
        }
    };
}

/// A UUID: 16 raw bytes (RFC 9562 byte order), contract type `uuid`. A
/// payload field holds it as `#[aster(uuid)] id: [u8; 16]`
/// (`Uuid::from(field)`, `field = uuid.into()`). Its string form is the hyphenated lowercase
/// `8-4-4-4-12`; [`FromStr`] also accepts upper case and the 32-digit form
/// without hyphens. The version and variant bits are carried as they are.
#[derive(Clone, Copy, Default, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Uuid([u8; 16]);

/// A ULID: 16 bytes (48-bit big-endian millisecond timestamp, then 80 random
/// bits), contract type `ulid`: the same 16-byte wire as [`Uuid`] under a
/// distinct contract identity. A payload field holds it as
/// `#[aster(ulid)] id: [u8; 16]`. Its string form is 26
/// characters of Crockford base32 (upper case; [`FromStr`] is
/// case-insensitive).
#[derive(Clone, Copy, Default, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Ulid([u8; 16]);

id_type!(Uuid, "uuid");
id_type!(Ulid, "ulid");

impl fmt::Display for Uuid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let h = hex::encode(self.0);
        write!(
            f,
            "{}-{}-{}-{}-{}",
            &h[0..8],
            &h[8..12],
            &h[12..16],
            &h[16..20],
            &h[20..32]
        )
    }
}

impl FromStr for Uuid {
    type Err = ScalarError;

    fn from_str(s: &str) -> Result<Self, ScalarError> {
        let bad = || ScalarError(format!("uuid: not a UUID string: {s:?}"));
        let digits: String = match s.len() {
            36 => {
                let b = s.as_bytes();
                if [8, 13, 18, 23].iter().any(|&i| b[i] != b'-') {
                    return Err(bad());
                }
                s.chars().filter(|&c| c != '-').collect()
            }
            32 => s.to_string(),
            _ => return Err(bad()),
        };
        if digits.len() != 32 || !digits.bytes().all(|c| c.is_ascii_hexdigit()) {
            return Err(bad());
        }
        let mut out = [0u8; 16];
        hex::decode_to_slice(&digits, &mut out).map_err(|_| bad())?;
        Ok(Uuid(out))
    }
}

const CROCKFORD: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";

impl Ulid {
    /// The ULID's 48-bit millisecond timestamp.
    pub fn timestamp_ms(&self) -> u64 {
        (self.to_u128() >> 80) as u64
    }
}

impl fmt::Display for Ulid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let n = self.to_u128();
        let mut out = [0u8; 26];
        for (i, c) in out.iter_mut().enumerate() {
            *c = CROCKFORD[((n >> (125 - 5 * i)) & 31) as usize];
        }
        f.write_str(std::str::from_utf8(&out).expect("ascii"))
    }
}

impl FromStr for Ulid {
    type Err = ScalarError;

    fn from_str(s: &str) -> Result<Self, ScalarError> {
        let bad = || {
            ScalarError(format!(
                "ulid: not a 26-character Crockford base32 ULID: {s:?}"
            ))
        };
        if s.len() != 26 {
            return Err(bad());
        }
        let mut n: u128 = 0;
        for (i, c) in s.bytes().enumerate() {
            let c = c.to_ascii_uppercase();
            let v = CROCKFORD.iter().position(|&a| a == c).ok_or_else(bad)? as u128;
            // The first character carries the top 3 of 128 bits.
            if i == 0 && v > 7 {
                return Err(bad());
            }
            n = (n << 5) | v;
        }
        Ok(Ulid::from_u128(n))
    }
}

// ── decimal(p, s) ────────────────────────────────────────────────────────────

/// The largest precision a `decimal(p, s)` may declare.
pub const MAX_DECIMAL_PRECISION: u32 = 1000;

fn pow10(n: u32) -> BigUint {
    BigUint::from(10u32).pow(n)
}

/// Rescale `value` to exactly `scale` digits after the point and check it has
/// at most `precision` digits: the `decimal(p, s)` rule every binding applies
/// on encode and on decode. Padding with zeros is exact; a value whose
/// dropped digits are not all zero (it would need rounding) or whose rescaled
/// unscaled value has more than `precision` digits is refused.
pub fn rescale_decimal(
    value: &Decimal,
    precision: u32,
    scale: u32,
) -> Result<Decimal, ScalarError> {
    let target = i64::from(scale);
    let have = i64::from(value.scale);
    let zero = BigInt::from(0);
    let unscaled = if have > target {
        let factor = BigInt::from(pow10((have - target) as u32));
        if &value.unscaled % &factor != zero {
            return Err(ScalarError(format!(
                "decimal({precision},{scale}): {} needs rounding to {scale} decimal places",
                DecimalDisplay(value)
            )));
        }
        &value.unscaled / &factor
    } else {
        &value.unscaled * BigInt::from(pow10((target - have) as u32))
    };
    if unscaled.magnitude() >= &pow10(precision) {
        return Err(ScalarError(format!(
            "decimal({precision},{scale}): {} has more than {precision} digits",
            DecimalDisplay(value)
        )));
    }
    Ok(Decimal::new(unscaled, scale as i32))
}

/// Plain decimal notation of a Fory [`Decimal`] (`-12.3400`, `1500`).
struct DecimalDisplay<'a>(&'a Decimal);

impl fmt::Display for DecimalDisplay<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let d = self.0;
        let sign = if d.unscaled.sign() == Sign::Minus {
            "-"
        } else {
            ""
        };
        let digits = d.unscaled.magnitude().to_string();
        if d.scale <= 0 {
            return write!(
                f,
                "{sign}{digits}{}",
                "0".repeat(d.scale.unsigned_abs() as usize)
            );
        }
        let scale = d.scale as usize;
        let digits = format!("{digits:0>width$}", width = scale + 1);
        let (int, frac) = digits.split_at(digits.len() - scale);
        write!(f, "{sign}{int}.{frac}")
    }
}

fn parse_decimal(s: &str) -> Result<Decimal, ScalarError> {
    let bad = || ScalarError(format!("decimal: not a decimal string: {s:?}"));
    let (neg, body) = match s.as_bytes().first() {
        Some(b'-') => (true, &s[1..]),
        Some(b'+') => (false, &s[1..]),
        _ => (false, s),
    };
    let (int, frac) = body.split_once('.').unwrap_or((body, ""));
    if int.is_empty() && frac.is_empty()
        || !int.bytes().chain(frac.bytes()).all(|c| c.is_ascii_digit())
    {
        return Err(bad());
    }
    let mut unscaled: BigInt = format!("{int}{frac}0").parse().map_err(|_| bad())?;
    unscaled /= BigInt::from(10);
    if neg {
        unscaled = -unscaled;
    }
    let scale = i32::try_from(frac.len()).map_err(|_| bad())?;
    Ok(Decimal::new(unscaled, scale))
}

/// A decimal constrained to precision `P` (total digits, `1..=1000`) and scale
/// `S` (digits after the point, `0..=P`): contract type `decimal(P,S)`, Fory
/// `DECIMAL` with scale exactly `S`.
///
/// Every value holds the invariant, so writing never fails: [`new`] and
/// [`FromStr`] rescale by zero padding and refuse a value that would need
/// rounding or has more than `P` digits, and reading applies the same rule to
/// the arriving value (a peer may send any scale that rescales exactly).
///
/// [`new`]: FixedDecimal::new
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct FixedDecimal<const P: u32, const S: u32>(Decimal);

impl<const P: u32, const S: u32> FixedDecimal<P, S> {
    const VALID: () = assert!(
        P >= 1 && P <= MAX_DECIMAL_PRECISION && S <= P,
        "decimal(P, S) needs 1 <= P <= 1000 and S <= P"
    );

    /// `value` rescaled to scale `S`, or the reason it does not fit.
    pub fn new(value: Decimal) -> Result<Self, ScalarError> {
        let () = Self::VALID;
        rescale_decimal(&value, P, S).map(FixedDecimal)
    }

    /// `unscaled × 10^-S`, or an error when it has more than `P` digits.
    pub fn from_unscaled(unscaled: BigInt) -> Result<Self, ScalarError> {
        Self::new(Decimal::new(unscaled, S as i32))
    }

    /// The value, at scale `S`.
    pub fn as_decimal(&self) -> &Decimal {
        &self.0
    }

    /// The value, at scale `S`.
    pub fn into_decimal(self) -> Decimal {
        self.0
    }

    /// The unscaled integer: the value is `unscaled × 10^-S`.
    pub fn unscaled(&self) -> &BigInt {
        &self.0.unscaled
    }

    /// The canonical contract type name, `decimal(P,S)`.
    pub fn type_primitive() -> String {
        let () = Self::VALID;
        format!("decimal({P},{S})")
    }
}

impl<const P: u32, const S: u32> Default for FixedDecimal<P, S> {
    fn default() -> Self {
        let () = Self::VALID;
        FixedDecimal(Decimal::new(BigInt::from(0), S as i32))
    }
}

impl<const P: u32, const S: u32> TryFrom<Decimal> for FixedDecimal<P, S> {
    type Error = ScalarError;

    fn try_from(value: Decimal) -> Result<Self, ScalarError> {
        Self::new(value)
    }
}

impl<const P: u32, const S: u32> From<FixedDecimal<P, S>> for Decimal {
    fn from(value: FixedDecimal<P, S>) -> Self {
        value.0
    }
}

impl<const P: u32, const S: u32> FromStr for FixedDecimal<P, S> {
    type Err = ScalarError;

    /// Plain decimal notation (`-12.5`, `+3`, `.25`); no exponent.
    fn from_str(s: &str) -> Result<Self, ScalarError> {
        Self::new(parse_decimal(s)?)
    }
}

impl<const P: u32, const S: u32> fmt::Display for FixedDecimal<P, S> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        DecimalDisplay(&self.0).fmt(f)
    }
}

impl<const P: u32, const S: u32> fmt::Debug for FixedDecimal<P, S> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "FixedDecimal<{P},{S}>({self})")
    }
}

impl<const P: u32, const S: u32> Serializer for FixedDecimal<P, S> {
    type Target = Self;

    const READ_DATA_ALWAYS_ADVANCES: bool = true;

    fn write_data(value: &Self, context: &mut WriteContext) -> Result<(), Error> {
        <Decimal as Serializer>::write_data(&value.0, context)
    }

    fn read_data(context: &mut ReadContext) -> Result<Self, Error> {
        let value = <Decimal as Serializer>::read_data(context)?;
        Self::new(value).map_err(|e| Error::invalid_data(e.to_string()))
    }

    fn default_value(_: &mut ReadContext) -> Result<Self, Error> {
        Ok(Self::default())
    }

    fn static_type_id() -> TypeId {
        TypeId::DECIMAL
    }

    fn write_type_info(context: &mut WriteContext) -> Result<(), Error> {
        context.writer.write_var_u32(TypeId::DECIMAL as u32);
        Ok(())
    }

    fn read_type_info(context: &mut ReadContext) -> Result<(), Error> {
        read_type_id(context, TypeId::DECIMAL)
    }
}

impl<const P: u32, const S: u32> WireField for FixedDecimal<P, S> {
    fn leaf() -> Leaf {
        Leaf::primitive_owned(Self::type_primitive())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uuid_string_forms() {
        let u: Uuid = "0190F2A8-1B2C-7D3E-8F40-123456789ABC".parse().unwrap();
        assert_eq!(u.to_string(), "0190f2a8-1b2c-7d3e-8f40-123456789abc");
        assert_eq!(
            "0190f2a81b2c7d3e8f40123456789abc".parse::<Uuid>().unwrap(),
            u
        );
        for bad in [
            "",
            "0190f2a8-1b2c-7d3e-8f40-123456789ab",
            "0190f2a8_1b2c-7d3e-8f40-123456789abc",
            "g190f2a81b2c7d3e8f40123456789abc",
        ] {
            assert!(bad.parse::<Uuid>().is_err(), "{bad}");
        }
        assert!(Uuid::try_from(&[0u8; 15][..]).is_err());
    }

    #[test]
    fn ulid_crockford_round_trip() {
        // The ULID spec's example: 01ARZ3NDEKTSV4RRFFQ69G5FAV.
        let u: Ulid = "01arz3ndektsv4rrffq69g5fav".parse().unwrap();
        assert_eq!(u.to_string(), "01ARZ3NDEKTSV4RRFFQ69G5FAV");
        assert_eq!(u.timestamp_ms(), 1_469_922_850_259);
        assert_eq!(
            Ulid::from_u128(u128::MAX).to_string(),
            "7ZZZZZZZZZZZZZZZZZZZZZZZZZ"
        );
        assert!(
            "8ZZZZZZZZZZZZZZZZZZZZZZZZZ".parse::<Ulid>().is_err(),
            "overflows 128 bits"
        );
        assert!(
            "01ARZ3NDEKTSV4RRFFQ69G5FAU".parse::<Ulid>().is_err(),
            "U is not Crockford"
        );
        assert!("01ARZ3NDEKTSV4RRFFQ69G5FA".parse::<Ulid>().is_err());
    }

    fn d(s: &str) -> Decimal {
        parse_decimal(s).unwrap()
    }

    #[test]
    fn decimal_rescale_rule() {
        type Money = FixedDecimal<18, 4>;
        let pad: Money = "1.5".parse().unwrap();
        assert_eq!(pad.as_decimal(), &Decimal::new(BigInt::from(15000), 4));
        assert_eq!(pad.to_string(), "1.5000");
        // Trailing zeros beyond the scale drop exactly.
        assert_eq!("1.500000".parse::<Money>().unwrap(), pad);
        // A negative wire scale pads too.
        assert_eq!(
            Money::new(Decimal::new(BigInt::from(15), -2))
                .unwrap()
                .to_string(),
            "1500.0000"
        );
        assert!("1.23456"
            .parse::<Money>()
            .unwrap_err()
            .to_string()
            .contains("rounding"));
        assert!("123456789012345"
            .parse::<Money>()
            .unwrap_err()
            .to_string()
            .contains("more than 18"));
        assert_eq!(
            "99999999999999.9999".parse::<Money>().unwrap().to_string(),
            "99999999999999.9999"
        );
        assert_eq!("-0.0001".parse::<Money>().unwrap().to_string(), "-0.0001");
        assert_eq!(Money::default().to_string(), "0.0000");
        assert_eq!(Money::type_primitive(), "decimal(18,4)");
        assert_eq!(FixedDecimal::<10, 0>::type_primitive(), "decimal(10,0)");
        assert_eq!(
            rescale_decimal(&d("0.05"), 1, 2).unwrap(),
            Decimal::new(BigInt::from(5), 2)
        );
        assert!(rescale_decimal(&d("0.10"), 1, 2).is_err());
        assert!(parse_decimal("1e5").is_err());
        assert!(parse_decimal(".").is_err());
    }
}
