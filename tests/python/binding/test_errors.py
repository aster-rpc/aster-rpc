"""
Binding-layer tests for the exception hierarchy.

Verifies that all error types are properly exported, are real Python
exception classes, and form the correct inheritance chain.  A wrong
`create_exception!` call or a missing `m.add(...)` registration would
fail these tests before any network code is exercised.
"""

import pytest
from aster_python import (
    IrohError,
    BlobNotFound,
    DocNotFound,
    ConnectionError,
    TicketError,
)


# ---------------------------------------------------------------------------
# Type identity
# ---------------------------------------------------------------------------

def test_iroh_error_is_exception():
    assert issubclass(IrohError, Exception)


def test_blob_not_found_is_iroh_error():
    assert issubclass(BlobNotFound, IrohError)


def test_doc_not_found_is_iroh_error():
    assert issubclass(DocNotFound, IrohError)


def test_connection_error_is_iroh_error():
    assert issubclass(ConnectionError, IrohError)


def test_ticket_error_is_iroh_error():
    assert issubclass(TicketError, IrohError)


# ---------------------------------------------------------------------------
# Can be raised and caught
# ---------------------------------------------------------------------------

def test_raise_and_catch_iroh_error():
    with pytest.raises(IrohError, match="test message"):
        raise IrohError("test message")


def test_blob_not_found_caught_as_iroh_error():
    with pytest.raises(IrohError):
        raise BlobNotFound("hash not found")


def test_doc_not_found_caught_as_iroh_error():
    with pytest.raises(IrohError):
        raise DocNotFound("doc not found")


def test_connection_error_caught_as_iroh_error():
    with pytest.raises(IrohError):
        raise ConnectionError("connection failed")


def test_ticket_error_caught_as_iroh_error():
    with pytest.raises(IrohError):
        raise TicketError("bad ticket")


# ---------------------------------------------------------------------------
# Specific types are distinguishable from each other
# ---------------------------------------------------------------------------

def test_blob_not_found_not_caught_as_doc_not_found():
    with pytest.raises(BlobNotFound):
        try:
            raise BlobNotFound("x")
        except DocNotFound:
            pytest.fail("BlobNotFound should not be caught as DocNotFound")


def test_error_message_preserved():
    msg = "detailed error description"
    try:
        raise IrohError(msg)
    except IrohError as e:
        assert msg in str(e)


# ---------------------------------------------------------------------------
# All types are distinct classes (not the same object)
# ---------------------------------------------------------------------------

def test_all_types_distinct():
    types = [IrohError, BlobNotFound, DocNotFound, ConnectionError, TicketError]
    assert len(set(id(t) for t in types)) == len(types)
