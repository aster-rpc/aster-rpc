"""Field names (``ffi_spec/Aster-FieldNaming.md``).

A field's contract name is the Python attribute name, verbatim, or its
:func:`aster.wire_name` override when the contract is defined in another
language. pyfory matches fields by its snake_case form of the attribute name,
so contract definition refuses a name that is not plain, an attribute whose
pyfory form differs from the reference snake_case form of its name (pyfory
writes ``type_`` as ``type``: name it ``type_ = wire_name("type", ...)``), and
two fields with the same snake_case form.
"""

from __future__ import annotations

import dataclasses
from collections.abc import Sequence

WIRE_NAME_KEY = "aster.wire_name"


def contract_field_name(f: dataclasses.Field) -> str:
    """A dataclass field's contract name: its :func:`aster.wire_name`, else its name."""
    return f.metadata.get(WIRE_NAME_KEY) or f.name


def pyfory_snake_case(name: str) -> str:
    """pyfory's snake_case form of an attribute name (what it writes on the wire)."""
    from pyfory.struct import _to_snake_case

    return _to_snake_case(name)


def _upper(c: str) -> bool:
    return "A" <= c <= "Z"


def _lower_or_digit(c: str) -> bool:
    return "a" <= c <= "z" or "0" <= c <= "9"


def _non_ascii_upper(c: str) -> bool:
    return ord(c) > 127 and c.isupper()


def reference_snake_case(name: str) -> str:
    """The reference snake_case form (Rust fory-core / Fory JavaScript)."""
    out: list[str] = []
    for i, c in enumerate(name):
        if _upper(c):
            if i > 0:
                prev_upper = _upper(name[i - 1])
                next_upper_or_end = i + 1 >= len(name) or _upper(name[i + 1])
                if not prev_upper or not next_upper_or_end:
                    out.append("_")
            out.append(c.lower())
        else:
            out.append(c)
    return "".join(out)


def not_plain(name: str) -> str | None:
    """Why ``name`` is not plain, or ``None`` when it is.

    Plain: every ASCII capital starts the name or follows an ASCII lowercase
    letter or a digit, no non-ASCII letter is uppercase, and the name does not
    end in ``_``.
    """
    for i, c in enumerate(name):
        if _upper(c) and i > 0 and not _lower_or_digit(name[i - 1]):
            return "an uppercase letter follows another uppercase letter, `_` or a non-ASCII character"
        if _non_ascii_upper(c):
            return "it has an uppercase letter outside ASCII"
    if name.endswith("_"):
        return "it ends in `_`"
    return None


def plain_suggestion(name: str) -> str:
    """A plain spelling of ``name`` (``userID`` -> ``userId``, ``HTTPServer`` -> ``HttpServer``)."""
    rs = list(name)
    i = 0
    while i < len(rs):
        if not _upper(rs[i]):
            if _non_ascii_upper(rs[i]):
                rs[i] = rs[i].lower()
            i += 1
            continue
        end = i
        while end < len(rs) and _upper(rs[end]):
            end += 1
        next_lower = end < len(rs) and "a" <= rs[end] <= "z"
        keep_last = next_lower and end - i >= 3
        first_ok = i == 0 or _lower_or_digit(rs[i - 1])
        for k in range(i, end):
            if not ((k == i and first_ok) or (k == end - 1 and keep_last)):
                rs[k] = rs[k].lower()
        i = end
    return "".join(rs).rstrip("_")


def check_field_names(fields: Sequence[tuple[str, str]]) -> None:
    """Check one type's fields, each as (pyfory form of the member, contract name).

    Raises ``TypeError`` naming the first offending field.
    """
    for member_snake, name in fields:
        why = not_plain(name)
        if why:
            raise TypeError(
                f"{name!r} is not a plain field name: {why}, so the Fory implementations "
                f"disagree on its snake_case form. Use a plain spelling such as "
                f"{plain_suggestion(name)!r} (ffi_spec/Aster-FieldNaming.md §5)"
            )
        snake = reference_snake_case(name)
        if snake != member_snake:
            raise TypeError(
                f"the field name {name!r} does not match its member: Fory writes the member "
                f"as {member_snake!r} but {name!r} is {snake!r}. Name the member so its "
                f"snake_case form is {snake!r}, with wire_name({name!r}) "
                f"(ffi_spec/Aster-FieldNaming.md §4)"
            )
    for i, (_, b) in enumerate(fields):
        for _, a in fields[:i]:
            if reference_snake_case(a) == reference_snake_case(b):
                raise TypeError(
                    f"the fields {a!r} and {b!r} collide: Fory writes both as "
                    f"{reference_snake_case(b)!r} and could not tell them apart "
                    f"(ffi_spec/Aster-FieldNaming.md §4)"
                )


def check_dataclass(cls: type) -> None:
    """Refuse ``cls``'s field names per Aster-FieldNaming.md §4 and §5."""
    try:
        check_field_names(
            [(pyfory_snake_case(f.name), contract_field_name(f)) for f in dataclasses.fields(cls)]
        )
    except TypeError as e:
        raise TypeError(f"{cls.__qualname__}: {e}") from None
