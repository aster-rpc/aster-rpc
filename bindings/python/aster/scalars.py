"""Canonical ``uuid`` / ``ulid`` / ``decimal(p, s)`` payload types.

Aster-ContractIdentity.md, "Time, UUID, and Decimal":

- ``uuid.UUID`` publishes as ``uuid``; :class:`ULID` as ``ulid`` (a distinct
  contract identity). Both are 16 raw bytes on the wire, Fory BINARY of
  length 16; a decoder refuses any other length.
- ``aster.decimal(p, s)`` is a field annotation publishing ``decimal(p,s)``:
  Fory DECIMAL with scale exactly ``s``. Encoding rescales a value to scale
  ``s`` by zero padding and refuses one that would need rounding or has more
  than ``p`` digits; decoding applies the same rule. A plain
  ``decimal.Decimal`` annotation is the unconstrained ``decimal``.

pyfory writes a field by its Fory type id, so these types reach pyfory as the
built-in ``bytes`` / ``Decimal`` carriers: :func:`install_pyfory` tells a
``pyfory.Fory`` that the annotations are BINARY / DECIMAL fields, and
:class:`WireAdapters` converts the values on the way in and out of the codec
(``uuid.UUID`` -> 16 bytes, the ``(p, s)`` check, and back).
"""

from __future__ import annotations

import collections.abc as _abc
import copy
import dataclasses
import decimal as _decimal
import os
import time
import types as _types
import typing
import uuid as _uuid
from typing import Any, Callable, Iterable

__all__ = [
    "MAX_DECIMAL_PRECISION",
    "DecimalSpec",
    "ScalarError",
    "ULID",
    "WireAdapters",
    "decimal",
    "decimal_spec",
    "install_pyfory",
    "type_primitive",
]

MAX_DECIMAL_PRECISION = 1000


class ScalarError(ValueError):
    """A value outside a canonical scalar type: an identifier that is not 16
    bytes, a malformed ULID / UUID string, or a decimal that does not fit its
    declared precision and scale."""


# ── ULID ──────────────────────────────────────────────────────────────────────

_CROCKFORD = "0123456789ABCDEFGHJKMNPQRSTVWXYZ"
_CROCKFORD_INDEX = {c: i for i, c in enumerate(_CROCKFORD)}


class ULID:
    """A ULID: 16 bytes (48-bit big-endian millisecond timestamp, then 80
    random bits), contract type ``ulid``. ``str()`` is 26 characters of
    Crockford base32; parsing is case-insensitive."""

    __slots__ = ("_bytes",)

    def __init__(self, value: "str | bytes | ULID") -> None:
        if isinstance(value, ULID):
            raw = value._bytes
        elif isinstance(value, (bytes, bytearray, memoryview)):
            raw = bytes(value)
            if len(raw) != 16:
                raise ScalarError(f"ulid: expected 16 bytes, got {len(raw)}")
        elif isinstance(value, str):
            raw = _parse_ulid(value)
        else:
            raise TypeError(f"ULID() takes str or 16 bytes, not {type(value).__name__}")
        object.__setattr__(self, "_bytes", raw)

    @classmethod
    def from_int(cls, n: int) -> "ULID":
        return cls(n.to_bytes(16, "big"))

    @classmethod
    def generate(cls, timestamp_ms: int | None = None) -> "ULID":
        """A new ULID for *timestamp_ms* (default now) with 80 random bits."""
        ms = int(time.time() * 1000) if timestamp_ms is None else timestamp_ms
        return cls(ms.to_bytes(6, "big") + os.urandom(10))

    @property
    def bytes(self) -> bytes:
        return self._bytes

    @property
    def int(self) -> int:
        return int.from_bytes(self._bytes, "big")

    @property
    def timestamp_ms(self) -> int:
        return int.from_bytes(self._bytes[:6], "big")

    def __str__(self) -> str:
        n = self.int
        return "".join(_CROCKFORD[(n >> (125 - 5 * i)) & 31] for i in range(26))

    def __repr__(self) -> str:
        return f"ULID('{self}')"

    def __eq__(self, other: object) -> bool:
        return isinstance(other, ULID) and other._bytes == self._bytes

    def __lt__(self, other: "ULID") -> bool:
        if not isinstance(other, ULID):
            return NotImplemented
        return self._bytes < other._bytes

    def __hash__(self) -> int:
        return hash((ULID, self._bytes))

    def __setattr__(self, name: str, value: Any) -> None:
        raise AttributeError("ULID is immutable")

    def __reduce__(self) -> tuple[Any, ...]:
        return (ULID, (self._bytes,))

    def __copy__(self) -> "ULID":
        return self

    def __deepcopy__(self, memo: dict) -> "ULID":
        return self


def _parse_ulid(s: str) -> bytes:
    if len(s) != 26:
        raise ScalarError(f"ulid: not a 26-character Crockford base32 ULID: {s!r}")
    n = 0
    for i, c in enumerate(s.upper()):
        v = _CROCKFORD_INDEX.get(c)
        # The first character carries the top 3 of 128 bits.
        if v is None or (i == 0 and v > 7):
            raise ScalarError(f"ulid: not a 26-character Crockford base32 ULID: {s!r}")
        n = (n << 5) | v
    return n.to_bytes(16, "big")


# ── decimal(p, s) ─────────────────────────────────────────────────────────────


@dataclasses.dataclass(frozen=True)
class DecimalSpec:
    """The declared precision (total digits) and scale (digits after the
    point) of a ``decimal(p, s)``."""

    precision: int
    scale: int

    def __post_init__(self) -> None:
        p, s = self.precision, self.scale
        if not (type(p) is int and type(s) is int and 1 <= p <= MAX_DECIMAL_PRECISION and 0 <= s <= p):
            raise ValueError(
                f"decimal({p!r}, {s!r}): needs integers 1 <= p <= {MAX_DECIMAL_PRECISION} and 0 <= s <= p"
            )

    @property
    def type_primitive(self) -> str:
        """The canonical contract name, ``decimal(p,s)`` (no spaces)."""
        return f"decimal({self.precision},{self.scale})"

    def rescale(self, value: Any) -> _decimal.Decimal:
        """*value* at exactly ``scale`` digits after the point, or
        :class:`ScalarError` when that needs rounding or leaves more than
        ``precision`` digits. Accepts ``Decimal``, ``int`` and decimal
        strings (never ``float``, which is not exact)."""
        if isinstance(value, bool) or isinstance(value, float):
            raise TypeError(f"{self.type_primitive}: expected Decimal, int or str, not {type(value).__name__}")
        if not isinstance(value, _decimal.Decimal):
            try:
                value = _decimal.Decimal(value)
            except (_decimal.InvalidOperation, TypeError, ValueError):
                raise ScalarError(f"{self.type_primitive}: not a decimal: {value!r}") from None
        if not value.is_finite():
            raise ScalarError(f"{self.type_primitive}: {value} is not finite")
        sign, digits, exponent = value.as_tuple()
        unscaled = int("".join(map(str, digits)) or "0")
        have = -exponent
        if have > self.scale:
            factor = 10 ** (have - self.scale)
            if unscaled % factor:
                raise ScalarError(
                    f"{self.type_primitive}: {value} needs rounding to {self.scale} decimal places"
                )
            unscaled //= factor
        else:
            unscaled *= 10 ** (self.scale - have)
        if unscaled >= 10**self.precision:
            raise ScalarError(f"{self.type_primitive}: {value} has more than {self.precision} digits")
        if unscaled == 0:
            sign = 0
        return _decimal.Decimal((sign, tuple(int(d) for d in str(unscaled)), -self.scale))


_DECIMAL_TYPES: dict[tuple[int, int], type] = {}


def decimal(precision: int, scale: int) -> type:
    """The field annotation for ``decimal(precision, scale)``.

    Returns a ``decimal.Decimal`` subclass that marks the field; values are
    ordinary ``Decimal`` objects. ``aster.decimal(18, 4)`` is the same class
    each time it is called::

        Money = aster.decimal(18, 4)

        @wire_type("shop/Price")
        @dataclass
        class Price:
            amount: Money = Decimal("0")
    """
    spec = DecimalSpec(precision, scale)
    key = (spec.precision, spec.scale)
    tp = _DECIMAL_TYPES.get(key)
    if tp is None:
        tp = type(
            f"decimal_{precision}_{scale}",
            (_decimal.Decimal,),
            {"__slots__": (), "__aster_decimal__": spec, "__module__": __name__},
        )
        _DECIMAL_TYPES[key] = tp
    return tp


def decimal_spec(tp: Any) -> DecimalSpec | None:
    """The ``(p, s)`` of an ``aster.decimal(p, s)`` annotation, else ``None``."""
    spec = getattr(tp, "__aster_decimal__", None) if isinstance(tp, type) else None
    return spec if isinstance(spec, DecimalSpec) else None


def type_primitive(tp: Any) -> str | None:
    """The canonical ``type_primitive`` of the types this module defines:
    ``uuid``, ``ulid`` or ``decimal(p,s)``; ``None`` for anything else."""
    if tp is _uuid.UUID:
        return "uuid"
    if tp is ULID:
        return "ulid"
    spec = decimal_spec(tp)
    return spec.type_primitive if spec is not None else None


# ── pyfory integration ────────────────────────────────────────────────────────


def install_pyfory(fory: Any) -> None:
    """Make *fory* treat ``uuid.UUID`` / :class:`ULID` annotations as BINARY
    fields and ``aster.decimal(p, s)`` annotations as DECIMAL fields, with the
    built-in ``bytes`` / ``Decimal`` serializers. Call before registering
    types. The values must already be the carriers (:class:`WireAdapters`).

    pyfory 1.7 has no public hook for this: it picks a struct field's Fory type
    from the annotation's registered ``TypeInfo``, so an internal-style entry
    is added for each class (the wire id is never re-mapped, so reading a
    BINARY / DECIMAL value still yields ``bytes`` / ``Decimal``).
    """
    import pyfory
    from pyfory.type_id import TypeId

    resolver = getattr(fory, "type_resolver", None)
    if resolver is None:
        return  # not a pyfory.Fory (a test stand-in)
    if not isinstance(getattr(resolver, "_types_info", None), dict):
        raise RuntimeError(
            f"pyfory {pyfory.__version__}: TypeResolver._types_info is gone; "
            "aster.scalars.install_pyfory needs updating for this pyfory release"
        )
    carriers = [(bytes, TypeId.BINARY, (_uuid.UUID, ULID))]
    carriers.append((_decimal.Decimal, TypeId.DECIMAL, tuple(_DECIMAL_TYPES.values())))
    for carrier, type_id, classes in carriers:
        serializer = resolver.get_type_info(carrier).serializer
        for cls in classes:
            if cls in resolver._types_info:
                continue
            resolver._types_info[cls] = pyfory.TypeInfo(cls, type_id, 0xFFFFFFFF, serializer, None, None, False)
            # Populates the accelerated lookup the struct field walker reads.
            resolver.get_type_info(cls)


# Encode / decode converters: (to_carrier, from_carrier).
_Conv = tuple[Callable[[Any], Any], Callable[[Any], Any]]


def _uuid_to_bytes(v: Any) -> bytes:
    if isinstance(v, _uuid.UUID):
        return v.bytes
    if isinstance(v, str):
        return _uuid.UUID(v).bytes
    if isinstance(v, (bytes, bytearray)) and len(v) == 16:
        return bytes(v)
    raise TypeError(f"uuid field: expected uuid.UUID, not {v!r}")


def _uuid_from_bytes(v: Any) -> _uuid.UUID:
    if isinstance(v, _uuid.UUID):
        return v
    if len(v) != 16:
        raise ScalarError(f"uuid: expected 16 bytes, got {len(v)}")
    return _uuid.UUID(bytes=bytes(v))


def _ulid_to_bytes(v: Any) -> bytes:
    if isinstance(v, ULID):
        return v.bytes
    if isinstance(v, (str, bytes, bytearray)):
        return ULID(v).bytes
    raise TypeError(f"ulid field: expected aster.ULID, not {v!r}")


def _ulid_from_bytes(v: Any) -> ULID:
    if isinstance(v, ULID):
        return v
    if len(v) != 16:
        raise ScalarError(f"ulid: expected 16 bytes, got {len(v)}")
    return ULID(bytes(v))


_SEQUENCE_ORIGINS = (list, tuple, set, frozenset, _abc.Sequence, _abc.MutableSequence, _abc.Set, _abc.MutableSet)
_MAPPING_ORIGINS = (dict, _abc.Mapping, _abc.MutableMapping)


class WireAdapters:
    """Value conversions between the user-facing types of this module and the
    carriers pyfory writes, for the dataclasses that (transitively) contain
    them. Types that contain none get no adapter and cost nothing."""

    def __init__(self, types: Iterable[type]) -> None:
        classes = [t for t in types if isinstance(t, type) and dataclasses.is_dataclass(t)]
        hints = {cls: _hints(cls) for cls in classes}
        # A class needs converting when a field holds one of the canonical
        # types, or reaches (through any container) a class that does.
        direct: dict[type, bool] = {}
        refs: dict[type, set[type]] = {}
        for cls in classes:
            found: set[type] = set()
            direct[cls] = any(_scan(h, found) for h in hints[cls].values())
            refs[cls] = found
        self._needs = {cls for cls in classes if direct[cls]}
        changed = True
        while changed:
            changed = False
            for cls in classes:
                if cls not in self._needs and refs[cls] & self._needs:
                    self._needs.add(cls)
                    changed = True
        self._plans: dict[type, list[tuple[str, Callable[[Any], Any], Callable[[Any], Any]]]] = {}
        for cls in self._needs:
            plan = []
            for f in dataclasses.fields(cls):
                conv = self._conv(hints[cls].get(f.name, f.type))
                if conv is not None:
                    plan.append((f.name, conv[0], conv[1]))
            self._plans[cls] = plan

    def __bool__(self) -> bool:
        return bool(self._plans)

    def encode(self, obj: Any) -> Any:
        """*obj* with canonical-type values replaced by their carriers (a
        shallow copy of each converted dataclass; *obj* is not modified)."""
        return self._encode_obj(obj) if type(obj) in self._plans else obj

    def decode(self, obj: Any) -> Any:
        """*obj* (freshly decoded) with carriers converted back and checked,
        in place."""
        return self._decode_obj(obj) if type(obj) in self._plans else obj

    def _encode_obj(self, obj: Any) -> Any:
        plan = self._plans.get(type(obj))
        if plan is None:
            return obj
        out = copy.copy(obj)
        for name, enc, _ in plan:
            value = getattr(obj, name)
            try:
                object.__setattr__(out, name, enc(value))
            except ScalarError as e:
                raise ScalarError(f"{type(obj).__qualname__}.{name}: {e}") from None
        return out

    def _decode_obj(self, obj: Any) -> Any:
        plan = self._plans.get(type(obj))
        if plan is None:
            return obj
        for name, _, dec in plan:
            try:
                object.__setattr__(obj, name, dec(getattr(obj, name)))
            except ScalarError as e:
                raise ScalarError(f"{type(obj).__qualname__}.{name}: {e}") from None
        return obj

    def _conv(self, tp: Any) -> _Conv | None:
        if tp is _uuid.UUID:
            return _uuid_to_bytes, _uuid_from_bytes
        if tp is ULID:
            return _ulid_to_bytes, _ulid_from_bytes
        spec = decimal_spec(tp)
        if spec is not None:
            return spec.rescale, spec.rescale
        if isinstance(tp, type) and tp in self._needs:
            return self._encode_obj, self._decode_obj
        origin = typing.get_origin(tp)
        args = typing.get_args(tp)
        if origin is typing.Annotated:
            return self._conv(args[0])
        if origin is typing.Union or origin is _types.UnionType:
            non_none = [a for a in args if a is not type(None)]
            if len(non_none) == 1:
                inner = self._conv(non_none[0])
                if inner is None:
                    return None
                enc, dec = inner
                return (lambda v: None if v is None else enc(v)), (lambda v: None if v is None else dec(v))
            if any(isinstance(a, type) and a in self._needs for a in non_none):
                # A union of messages: convert whichever member arrived.
                return self._encode_obj, self._decode_obj
            return None
        if origin in _MAPPING_ORIGINS:
            if len(args) != 2:
                return None
            kc, vc = self._conv(args[0]), self._conv(args[1])
            if kc is None and vc is None:
                return None
            ke, kd = kc or (_same, _same)
            ve, vd = vc or (_same, _same)
            return (
                lambda m: None if m is None else {ke(k): ve(v) for k, v in m.items()},
                lambda m: None if m is None else {kd(k): vd(v) for k, v in m.items()},
            )
        if origin in _SEQUENCE_ORIGINS:
            elem = args[0] if args else None
            inner = self._conv(elem) if elem is not None else None
            if inner is None:
                return None
            enc, dec = inner
            return (lambda xs: _map_seq(xs, enc)), (lambda xs: _map_seq(xs, dec))
        return None


def _same(v: Any) -> Any:
    return v


def _map_seq(xs: Any, fn: Callable[[Any], Any]) -> Any:
    if xs is None:
        return None
    if isinstance(xs, (set, frozenset)):
        return type(xs)(fn(x) for x in xs)
    if isinstance(xs, tuple):
        return tuple(fn(x) for x in xs)
    return [fn(x) for x in xs]


def _hints(cls: type) -> dict[str, Any]:
    try:
        return typing.get_type_hints(cls)
    except Exception:
        return {f.name: f.type for f in dataclasses.fields(cls)}


def _scan(tp: Any, refs: set[type]) -> bool:
    """Whether *tp* holds a canonical type directly; collects the dataclasses
    it reaches into *refs*."""
    if tp is _uuid.UUID or tp is ULID or decimal_spec(tp) is not None:
        return True
    if isinstance(tp, type):
        if dataclasses.is_dataclass(tp):
            refs.add(tp)
        return False
    return any([_scan(a, refs) for a in typing.get_args(tp)])
