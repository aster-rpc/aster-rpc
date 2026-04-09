"""
Tests for aster.metadata -- Metadata type, decorator integration, and
contract identity independence.

Key invariant: Metadata MUST NOT affect contract identity (BLAKE3 hash).
"""

from __future__ import annotations

from dataclasses import dataclass

import pytest

from aster import Metadata, rpc, server_stream, client_stream, bidi_stream, service, wire_type
from aster.service import MethodInfo, ServiceInfo


# ── Contract identity independence ──────────────────────────────────────────────


class TestMetadataDoesNotAffectContractId:
    """Metadata must NOT change contract identity hashes."""

    def test_service_with_and_without_metadata_same_contract_id(self):
        """Adding metadata to @service must not change the contract ID."""
        from aster.contract.identity import (
            ServiceContract, canonical_xlang_bytes, compute_contract_id,
        )

        @wire_type("meta_test/Req1")
        @dataclass
        class Req1:
            value: str = ""

        @wire_type("meta_test/Resp1")
        @dataclass
        class Resp1:
            result: str = ""

        @service(name="NoMeta", version=1)
        class NoMetaService:
            @rpc
            async def do_thing(self, req: Req1) -> Resp1: ...

        @service(name="NoMeta", version=1, metadata=Metadata(description="A service with metadata"))
        class WithMetaService:
            @rpc(metadata=Metadata(description="Does a thing"))
            async def do_thing(self, req: Req1) -> Resp1: ...

        contract_no_meta = ServiceContract.from_service_info(NoMetaService.__aster_service_info__)
        contract_with_meta = ServiceContract.from_service_info(WithMetaService.__aster_service_info__)

        bytes_no_meta = canonical_xlang_bytes(contract_no_meta)
        bytes_with_meta = canonical_xlang_bytes(contract_with_meta)

        id_no_meta = compute_contract_id(bytes_no_meta)
        id_with_meta = compute_contract_id(bytes_with_meta)

        assert id_no_meta == id_with_meta, (
            f"Metadata changed contract ID! "
            f"without={id_no_meta[:16]}... with={id_with_meta[:16]}..."
        )

    def test_wire_type_field_metadata_does_not_affect_contract_id(self):
        """Adding field metadata to @wire_type must not change the contract ID."""
        from aster.contract.identity import (
            ServiceContract, canonical_xlang_bytes, compute_contract_id,
        )

        @wire_type("meta_test/PlainReq")
        @dataclass
        class PlainReq:
            amount: int = 0
            name: str = ""

        @wire_type("meta_test/PlainResp")
        @dataclass
        class PlainResp:
            ok: bool = False

        @wire_type("meta_test/AnnotatedReq", metadata={
            "amount": Metadata(description="Total in cents"),
            "name": Metadata(description="Customer name"),
        })
        @dataclass
        class AnnotatedReq:
            amount: int = 0
            name: str = ""

        @wire_type("meta_test/AnnotatedResp")
        @dataclass
        class AnnotatedResp:
            ok: bool = False

        @service(name="PlainSvc", version=1)
        class PlainSvc:
            @rpc
            async def do_it(self, req: PlainReq) -> PlainResp: ...

        @service(name="PlainSvc", version=1)
        class AnnotatedSvc:
            @rpc
            async def do_it(self, req: AnnotatedReq) -> AnnotatedResp: ...

        c1 = ServiceContract.from_service_info(PlainSvc.__aster_service_info__)
        c2 = ServiceContract.from_service_info(AnnotatedSvc.__aster_service_info__)

        id1 = compute_contract_id(canonical_xlang_bytes(c1))
        id2 = compute_contract_id(canonical_xlang_bytes(c2))

        # Same structure, different metadata -- same contract ID
        assert id1 == id2, (
            f"Field metadata changed contract ID! "
            f"plain={id1[:16]}... annotated={id2[:16]}..."
        )


# ── Decorator integration ────────────────────────────────────────────────────


class TestMetadataOnRpc:
    """Test @rpc metadata parameter."""

    def test_explicit_metadata_flows_to_method_info(self):
        meta = Metadata(description="Assign a task")

        @rpc(metadata=meta)
        async def assign_task(self, req): ...

        info: MethodInfo = assign_task.__aster_method_info__
        assert info.metadata is meta
        assert info.metadata.description == "Assign a task"

    def test_docstring_auto_capture(self):
        @rpc
        async def documented_method(self, req):
            """This method does something important.

            More details that should not be captured.
            """
            ...

        info: MethodInfo = documented_method.__aster_method_info__
        assert info.metadata is not None
        assert info.metadata.description == "This method does something important."

    def test_explicit_metadata_overrides_docstring(self):
        meta = Metadata(description="Explicit wins")

        @rpc(metadata=meta)
        async def method_with_both(self, req):
            """This docstring should be ignored."""
            ...

        info: MethodInfo = method_with_both.__aster_method_info__
        assert info.metadata.description == "Explicit wins"

    def test_no_metadata_no_docstring_is_none(self):
        @rpc
        async def bare_method(self, req): ...

        info: MethodInfo = bare_method.__aster_method_info__
        assert info.metadata is None


class TestMetadataOnServerStream:
    def test_metadata_flows_through(self):
        meta = Metadata(description="Watch for updates")

        @server_stream(metadata=meta)
        async def watch(self, req):
            yield

        info: MethodInfo = watch.__aster_method_info__
        assert info.metadata.description == "Watch for updates"

    def test_docstring_auto_capture(self):
        @server_stream
        async def watch_items(self, req):
            """Stream item updates in real time."""
            yield

        info: MethodInfo = watch_items.__aster_method_info__
        assert info.metadata is not None
        assert info.metadata.description == "Stream item updates in real time."


class TestMetadataOnClientStream:
    def test_metadata_flows_through(self):
        meta = Metadata(description="Upload batch")

        @client_stream(metadata=meta)
        async def upload(self, reqs): ...

        info: MethodInfo = upload.__aster_method_info__
        assert info.metadata.description == "Upload batch"


class TestMetadataOnBidiStream:
    def test_metadata_flows_through(self):
        meta = Metadata(description="Chat loop")

        @bidi_stream(metadata=meta)
        async def chat(self, reqs):
            yield

        info: MethodInfo = chat.__aster_method_info__
        assert info.metadata.description == "Chat loop"


class TestMetadataOnService:
    def test_explicit_metadata(self):
        @wire_type("meta_svc/Req")
        @dataclass
        class SvcReq:
            x: int = 0

        @wire_type("meta_svc/Resp")
        @dataclass
        class SvcResp:
            y: int = 0

        meta = Metadata(description="Manages billing operations")

        @service(name="BillingSvc", version=1, metadata=meta)
        class BillingSvc:
            @rpc
            async def charge(self, req: SvcReq) -> SvcResp: ...

        info: ServiceInfo = BillingSvc.__aster_service_info__
        assert info.metadata is meta
        assert info.metadata.description == "Manages billing operations"

    def test_docstring_auto_capture_on_service(self):
        @wire_type("meta_svc2/Req2")
        @dataclass
        class SvcReq2:
            x: int = 0

        @wire_type("meta_svc2/Resp2")
        @dataclass
        class SvcResp2:
            y: int = 0

        @service(name="DocSvc", version=1)
        class DocSvc:
            """Handles document processing workflows."""

            @rpc
            async def process(self, req: SvcReq2) -> SvcResp2: ...

        info: ServiceInfo = DocSvc.__aster_service_info__
        assert info.metadata is not None
        assert info.metadata.description == "Handles document processing workflows."


class TestWireTypeFieldMetadata:
    def test_field_metadata_stored_on_class(self):
        @wire_type("meta_field/Invoice", metadata={
            "amount": Metadata(description="Total in cents"),
            "currency": Metadata(description="ISO 4217 code"),
        })
        @dataclass
        class Invoice:
            amount: int = 0
            currency: str = "USD"

        field_meta = Invoice.__wire_type_field_metadata__
        assert field_meta["amount"].description == "Total in cents"
        assert field_meta["currency"].description == "ISO 4217 code"

    def test_no_metadata_no_attribute(self):
        @wire_type("meta_field/Simple")
        @dataclass
        class Simple:
            value: int = 0

        assert not hasattr(Simple, "__wire_type_field_metadata__")


class TestMetadataType:
    def test_default_empty_description(self):
        m = Metadata()
        assert m.description == ""

    def test_description_set(self):
        m = Metadata(description="hello")
        assert m.description == "hello"

    def test_is_wire_type(self):
        assert hasattr(Metadata, "__wire_type__")
        assert Metadata.__wire_type__ == "_aster/Metadata"


class TestFirstParagraphHelper:
    def test_single_line(self):
        from aster.decorators import _first_paragraph
        assert _first_paragraph("Hello world.") == "Hello world."

    def test_multi_paragraph(self):
        from aster.decorators import _first_paragraph
        doc = """First paragraph line one.
        First paragraph line two.

        Second paragraph should be ignored.
        """
        result = _first_paragraph(doc)
        assert result == "First paragraph line one. First paragraph line two."

    def test_none(self):
        from aster.decorators import _first_paragraph
        assert _first_paragraph(None) is None

    def test_empty(self):
        from aster.decorators import _first_paragraph
        assert _first_paragraph("") is None

    def test_whitespace_only(self):
        from aster.decorators import _first_paragraph
        assert _first_paragraph("   \n   \n   ") is None
