#![cfg(feature = "rpc")]
//! Byte fields: the contract and the Fory wire must agree.
//!
//! `#[derive(AsterType)]` publishes a byte buffer as `binary`, which Python
//! `bytes` and TypeScript `Uint8Array` write as Fory BINARY (type 41). Fory's
//! Rust derive writes an *unannotated* `Vec<u8>` as LIST<UINT8> (type 22), so
//! the derive refuses to compile one (see the `aster-macros` unit tests) and
//! requires `#[fory(bytes)]`. These tests pin the encoding that rule buys: an
//! `AsterType` struct with a byte field encodes to exactly the bytes pyfory
//! produces for the same `interop/EchoRequest` shape, and decodes pyfory's.
//!
//! Goldens were produced with the Python binding's codec (pyfory, xlang):
//!
//! ```python
//! @wire_type("interop/EchoRequest")
//! @dataclass
//! class EchoRequest:
//!     message: str = ""
//!     count: pyfory.Int32 = 0
//!     data: bytes = b""
//!
//! ForyCodec(types=[EchoRequest]).encode(EchoRequest(...)).hex()
//! ```

use aster::rpc::{AsterType, PayloadRegistry, WireField};
use fory_derive::ForyStruct;

/// `EchoRequest("", 7, b"\x00\x01\x02\xff")`. Fory orders the string field
/// last; its one-byte header is the only part that differs from Rust, because
/// pyfory flags an empty string latin-1 (`0x00`) and Rust UTF-8 (`0x02`).
const PY_EMPTY_MESSAGE: &str = "01ff1e0023a0530eb337c340e31521b3245cf025f4823bbb12428494\
                                c04c0589d46cc048290c130050153092900c400e04000102ff00";

/// `EchoRequest("hi", 7, b"\x00\x01\x02\xff")`. pyfory writes `"hi"` as a
/// latin-1 string (header `0x08`) where Rust writes UTF-8 (`0x0a`); every
/// reader accepts both, so this one is a decode golden.
const PY_HI: &str = "01ff1e0023a0530eb337c340e31521b3245cf025f4823bbb12428494\
                     c04c0589d46cc048290c130050153092900c400e04000102ff086869";

#[derive(ForyStruct, aster::AsterType, Debug, Default, Clone, PartialEq)]
#[aster(wire = "interop/EchoRequest")]
struct EchoRequest {
    message: String,
    count: i32,
    #[fory(bytes)]
    data: Vec<u8>,
}

/// Same wire tag and contract (`[u8; N]` is published as `binary` too):
/// Fory writes a fixed `u8` array as BINARY without any attribute.
#[derive(ForyStruct, aster::AsterType, Debug, Default, Clone, PartialEq)]
#[aster(wire = "interop/EchoRequest")]
struct EchoRequestFixed {
    message: String,
    count: i32,
    data: [u8; 4],
}

/// What the derive now rejects: an unannotated `Vec<u8>`. `ForyStruct` alone
/// still accepts it, which is how the wire mismatch looks.
#[derive(ForyStruct, Debug, Default, Clone, PartialEq)]
struct EchoRequestUnannotated {
    message: String,
    count: i32,
    data: Vec<u8>,
}

fn runtime<T: WireField + 'static>() -> fory_core::Fory {
    let mut reg = PayloadRegistry::new();
    T::register_payload(&mut reg);
    reg.into_fory()
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Compare an encoding with [`PY_EMPTY_MESSAGE`]: byte-for-byte, except the
/// final empty-string header, whose encoding flag is writer-specific.
fn assert_matches_python(rust: &[u8]) {
    let golden = unhex(PY_EMPTY_MESSAGE);
    assert_eq!(rust.len(), golden.len(), "{}", hex(rust));
    let n = golden.len() - 1;
    assert_eq!(hex(&rust[..n]), hex(&golden[..n]));
    assert_eq!(rust[n] >> 2, 0, "trailing field is the empty string");
}

fn unhex(s: &str) -> Vec<u8> {
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
        .collect()
}

const DATA: [u8; 4] = [0, 1, 2, 255];

#[test]
fn fory_bytes_field_encodes_exactly_like_python_bytes() {
    let fory = runtime::<EchoRequest>();
    let v = EchoRequest {
        message: String::new(),
        count: 7,
        data: DATA.to_vec(),
    };
    assert_matches_python(&fory.serialize(&v).unwrap());
}

#[test]
fn fory_bytes_field_decodes_python_bytes() {
    let fory = runtime::<EchoRequest>();
    for (golden, message) in [(PY_EMPTY_MESSAGE, ""), (PY_HI, "hi")] {
        let back: EchoRequest = fory.deserialize(&unhex(golden)).unwrap();
        assert_eq!(
            back,
            EchoRequest {
                message: message.into(),
                count: 7,
                data: DATA.to_vec(),
            }
        );
    }
}

#[test]
fn fixed_u8_array_field_is_fory_binary() {
    assert_eq!(
        EchoRequestFixed::aster_type_hash(),
        EchoRequest::aster_type_hash(),
        "`[u8; N]` and `#[fory(bytes)] Vec<u8>` publish the same contract type"
    );
    let fory = runtime::<EchoRequestFixed>();
    let v = EchoRequestFixed {
        message: String::new(),
        count: 7,
        data: DATA,
    };
    assert_matches_python(&fory.serialize(&v).unwrap());
    let back: EchoRequestFixed = fory.deserialize(&unhex(PY_HI)).unwrap();
    assert_eq!(back.data, DATA);
}

#[test]
fn unannotated_vec_u8_is_not_python_bytes_on_the_wire() {
    // Regression evidence for why the derive rejects this shape.
    let mut reg = PayloadRegistry::new();
    reg.register::<EchoRequestUnannotated>("interop.EchoRequest");
    let fory = reg.into_fory();
    let v = EchoRequestUnannotated {
        message: String::new(),
        count: 7,
        data: DATA.to_vec(),
    };
    let rust = fory.serialize(&v).unwrap();
    let golden = unhex(PY_EMPTY_MESSAGE);
    assert_ne!(rust[..rust.len() - 1], golden[..golden.len() - 1]);
}

/// The opt-out: `#[aster(uint8_list)]` keeps Fory's LIST<UINT8> and says so
/// in the contract.
#[derive(ForyStruct, aster::AsterType, Debug, Default, Clone, PartialEq)]
#[aster(wire = "t/Samples")]
struct Samples {
    #[aster(uint8_list)]
    levels: Vec<u8>,
    #[fory(list(element(bytes)))]
    chunks: Vec<Vec<u8>>,
}

#[test]
fn uint8_list_and_list_of_binary_contract_shapes() {
    let td = Samples::aster_type_def();
    let f = |n: &str| td.fields.iter().find(|f| f.name == n).unwrap();
    assert_eq!(f("levels").type_primitive, "uint8");
    assert_eq!(format!("{:?}", f("levels").container), "List");
    assert_eq!(f("chunks").type_primitive, "binary");
    assert_eq!(format!("{:?}", f("chunks").container), "List");

    let fory = runtime::<Samples>();
    let v = Samples {
        levels: vec![1, 2, 3],
        chunks: vec![vec![], DATA.to_vec()],
    };
    let back: Samples = fory.deserialize(&fory.serialize(&v).unwrap()).unwrap();
    assert_eq!(back, v);
}
