"""Produce the ``pyfory_cases`` section of ``dynamic-payloads.json``.

The dataclasses below mirror the Rust golden types in
``aster/tests/dynamic_payload_vector.rs`` (same wire names, field names and
wire types), registered the way ``aster.codec.ForyCodec`` registers XLANG
types (``xlang=True, compatible=True, ref=False, strict=True``, by
``package.Name``). Each case records pyfory's bytes and the value in the Aster
JSON encoding; the Rust test checks that the dynamic codec decodes the bytes
to the value and, with ``StringEncoding::Pyfory``, encodes the value back to
exactly these bytes.

Run from the repository root (needs pyfory)::

    uv run --no-sync python conformance/vectors/dynamic-payloads-pyfory.py
"""

from __future__ import annotations

import base64
import enum
import json
import pathlib
from dataclasses import dataclass, field, fields, is_dataclass
from typing import Optional

import pyfory

VECTORS = pathlib.Path(__file__).with_name("dynamic-payloads.json")


@dataclass
class Inner:
    a: pyfory.Int32 = 0
    s: str = ""


@dataclass
class Scalars:
    flag: bool = False
    tiny: pyfory.Int8 = 0
    small: pyfory.Int16 = 0
    medium: pyfory.Int32 = 0
    large: pyfory.Int64 = 0
    ushort: pyfory.UInt16 = 0
    uint: pyfory.UInt32 = 0
    ulong: pyfory.UInt64 = 0
    ratio32: pyfory.Float32 = 0.0
    ratio64: float = 0.0
    text: str = ""
    blob: bytes = b""


@dataclass
class Optionals:
    maybe_int: Optional[pyfory.Int32] = None
    maybe_long: Optional[pyfory.Int64] = None
    maybe_flag: Optional[bool] = None
    maybe_ratio: Optional[float] = None
    maybe_text: Optional[str] = None
    maybe_inner: Optional[Inner] = None
    maybe_list: Optional[list[str]] = None


@dataclass
class Collections:
    names: list[str] = field(default_factory=list)
    counts: list[pyfory.Int32] = field(default_factory=list)
    longs: list[pyfory.Int64] = field(default_factory=list)
    flags: list[bool] = field(default_factory=list)
    ratios: list[float] = field(default_factory=list)
    inners: list[Inner] = field(default_factory=list)
    tags: set[str] = field(default_factory=set)
    scores: dict[str, pyfory.Int32] = field(default_factory=dict)
    by_id: dict[pyfory.Int32, str] = field(default_factory=dict)
    nested: dict[str, Inner] = field(default_factory=dict)


@dataclass
class Nested:
    first: Inner = field(default_factory=Inner)
    second: Inner = field(default_factory=Inner)
    more: list[Inner] = field(default_factory=list)
    label: str = ""


class Color(enum.IntEnum):
    Red = 0
    Green = 1
    Blue = 2


@dataclass
class Paint:
    c: Color = Color.Red
    oc: Optional[Color] = None
    lc: list[Color] = field(default_factory=list)


@dataclass
class Fixed:
    fi32: pyfory.FixedInt32 = 0
    fi64: pyfory.FixedInt64 = 0
    fu32: pyfory.FixedUInt32 = 0
    fu64: pyfory.FixedUInt64 = 0
    ti64: pyfory.TaggedInt64 = 0
    tu64: pyfory.TaggedUInt64 = 0
    u8: pyfory.UInt8 = 0


@dataclass
class EchoRequest:
    message: str = ""
    count: pyfory.Int32 = 0
    data: bytes = b""


@dataclass
class EchoReply:
    message: str = ""
    count: pyfory.Int32 = 0
    data: bytes = b""
    server_lang: str = ""


TYPES = [
    (Inner, "dyn.v1.Inner"),
    (Scalars, "dyn.v1.Scalars"),
    (Optionals, "dyn.v1.Optionals"),
    (Collections, "dyn.v1.Collections"),
    (Nested, "dyn.v1.Nested"),
    (Color, "dyn.v1.Color"),
    (Paint, "dyn.v1.Paint"),
    (Fixed, "dyn.v1.Fixed"),
    (EchoRequest, "interop.EchoRequest"),
    (EchoReply, "interop.EchoReply"),
]

# Fields whose wire type is 64-bit: rendered as decimal strings in Aster JSON.
WIDE = {"large", "ulong", "maybe_long", "fi64", "fu64", "ti64", "tu64"}


def to_json(v, name=""):
    """A Python value in the Aster JSON encoding."""
    if isinstance(v, enum.Enum):
        return v.name
    if is_dataclass(v):
        return {f.name: to_json(getattr(v, f.name), f.name) for f in fields(v)}
    if isinstance(v, bool) or v is None or isinstance(v, str):
        return v
    if isinstance(v, (bytes, bytearray)):
        return base64.urlsafe_b64encode(bytes(v)).decode().rstrip("=")
    if isinstance(v, int):
        return str(v) if name in WIDE or name == "longs" else v
    if isinstance(v, float):
        return v
    if isinstance(v, (list, tuple)):
        return [to_json(x, name) for x in v]
    if isinstance(v, (set, frozenset)):
        return [to_json(x, name) for x in sorted(v)]
    if isinstance(v, dict):
        if all(isinstance(k, str) for k in v):
            return {k: to_json(x) for k, x in v.items()}
        return [[to_json(k), to_json(x)] for k, x in v.items()]
    raise TypeError(type(v))


CASES = [
    ("inner latin1", "dyn.v1.Inner", Inner(5, "hi")),
    (
        "scalars",
        "dyn.v1.Scalars",
        Scalars(True, -1, 300, -7, 1 << 40, 65535, 7, 1 << 50, 1.5, -2.25, "caf\u00e9", b"\x00\x01\xff"),
    ),
    ("optionals all null", "dyn.v1.Optionals", Optionals()),
    (
        "optionals all set",
        "dyn.v1.Optionals",
        Optionals(3, -9_000_000_000, False, 0.1, "x", Inner(1, "y"), ["q", "r"]),
    ),
    (
        "collections",
        "dyn.v1.Collections",
        Collections(
            ["a", "b"],
            [1, -1, 300],
            [5, -5_000_000_000],
            [True, False],
            [1.0, -0.5],
            [Inner(1, ""), Inner(2, "two")],
            {"x"},
            {"k": 9, "m": -1},
            {-2: "minus two", 1: "one"},
            {"n": Inner(4, "four")},
        ),
    ),
    ("nested", "dyn.v1.Nested", Nested(Inner(1, "a"), Inner(2, "b"), [Inner(3, "c")], "L")),
    ("enum fields", "dyn.v1.Paint", Paint(Color.Blue, Color.Green, [Color.Red, Color.Blue])),
    ("enum fields, optional null", "dyn.v1.Paint", Paint(Color.Green, None, [])),
    (
        "fixed and tagged integers",
        "dyn.v1.Fixed",
        Fixed(-5, -(1 << 40), 4_000_000_000, (1 << 63) + 5, -7, 1 << 40, 250),
    ),
    ("interop EchoRequest latin1", "interop.EchoRequest", EchoRequest("h\u00e9llo", 3, b"\x01\x02\x03")),
    ("interop EchoRequest utf16", "interop.EchoRequest", EchoRequest("\u65e5\u672c\u8a9e", -1, b"")),
    ("interop EchoRequest utf8", "interop.EchoRequest", EchoRequest("a\U0001F600", 0, b"\xff")),
    ("interop EchoReply", "interop.EchoReply", EchoReply("hello", 4, b"\x01", "python")),
]


def main() -> None:
    fory = pyfory.Fory(xlang=True, ref=False, compatible=True, strict=True)
    for cls, name in TYPES:
        fory.register(cls, name=name)
    out = []
    for case_name, type_name, value in CASES:
        out.append(
            {
                "name": case_name,
                "type": type_name,
                "value": to_json(value),
                "hex": bytes(fory.serialize(value)).hex(),
                "producer": f"pyfory {pyfory.__version__}",
            }
        )
    doc = json.loads(VECTORS.read_text())
    doc["pyfory_cases"] = out
    VECTORS.write_text(json.dumps(doc, indent=2, ensure_ascii=False) + "\n")
    print(f"wrote {len(out)} pyfory cases to {VECTORS}")


if __name__ == "__main__":
    main()
