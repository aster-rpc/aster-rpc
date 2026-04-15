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


# ── Module-level wire types for manifest propagation tests ───────────────────
# These MUST be at module scope. With ``from __future__ import annotations``,
# ``typing.get_type_hints`` resolves request/response annotations through the
# method's ``__globals__`` -- locally-defined classes inside a test method
# are not reachable. Module-level types resolve correctly and give the same
# code path as real user service definitions.

from typing import Annotated  # noqa: E402
from aster import describe, Description  # noqa: E402


@wire_type("mfest_test/ReqDesc")
@dataclass
class _ReqDesc:
    name: str = describe("Name of user.", tags=["pii"])
    api_key: str = describe("API key.", tags=["secret"])
    plain: str = ""


@wire_type("mfest_test/RespDesc")
@dataclass
class _RespDesc:
    greeting: str = describe("Response greeting.")


@wire_type("mfest_test/ReqWtMeta", metadata={
    "amount": Metadata(description="In cents.", tags=["money"]),
})
@dataclass
class _ReqWtMeta:
    amount: int = 0


@wire_type("mfest_test/RespWtMeta")
@dataclass
class _RespWtMeta:
    ok: bool = False


@wire_type("mfest_test/InlineGreeting")
@dataclass
class _InlineGreeting:
    text: str = ""


@wire_type("mfest_test/PlainNoDesc")
@dataclass
class _PlainNoDesc:
    x: int = 0


@wire_type("mfest_test/RespNoDesc")
@dataclass
class _RespNoDesc:
    ok: bool = False


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

    def test_default_empty_tags(self):
        m = Metadata()
        assert m.tags == []
        assert m.deprecated is False
        assert m.since_version is None

    def test_description_set(self):
        m = Metadata(description="hello")
        assert m.description == "hello"

    def test_tags_deprecated_since_version_set(self):
        m = Metadata(
            description="hi",
            tags=["readonly", "cheap"],
            deprecated=True,
            since_version=3,
        )
        assert m.tags == ["readonly", "cheap"]
        assert m.deprecated is True
        assert m.since_version == 3

    def test_is_wire_type(self):
        assert hasattr(Metadata, "__wire_type__")
        assert Metadata.__wire_type__ == "_aster/Metadata"


class TestDescribeHelper:
    """Tests for ``aster.metadata.describe(...)`` field factory."""

    def test_describe_attaches_description(self):
        from aster import describe
        import dataclasses

        @dataclasses.dataclass
        class Thing:
            name: str = describe("Name to greet.")

        (fld,) = dataclasses.fields(Thing)
        assert fld.metadata["aster"]["description"] == "Name to greet."
        assert fld.metadata["aster"]["tags"] == []

    def test_describe_attaches_tags(self):
        from aster import describe
        import dataclasses

        @dataclasses.dataclass
        class Thing:
            api_key: str = describe("API key.", tags=["secret"])

        (fld,) = dataclasses.fields(Thing)
        assert fld.metadata["aster"]["description"] == "API key."
        assert fld.metadata["aster"]["tags"] == ["secret"]

    def test_describe_with_default(self):
        from aster import describe
        import dataclasses

        @dataclasses.dataclass
        class Thing:
            locale: str = describe("BCP 47.", default="en-US")

        t = Thing()
        assert t.locale == "en-US"

    def test_describe_with_default_factory(self):
        from aster import describe
        import dataclasses

        @dataclasses.dataclass
        class Thing:
            items: list = describe("Items.", default_factory=list)

        t = Thing()
        assert t.items == []

    def test_field_metadata_extraction(self):
        from aster.metadata import field_metadata
        from aster import describe
        import dataclasses

        @dataclasses.dataclass
        class Thing:
            name: str = describe("hello", tags=["pii"])
            plain: str = ""

        by_name = {f.name: f for f in dataclasses.fields(Thing)}
        desc, tags = field_metadata(by_name["name"])
        assert desc == "hello"
        assert tags == ["pii"]
        desc2, tags2 = field_metadata(by_name["plain"])
        assert desc2 == ""
        assert tags2 == []


class TestDescriptionMarker:
    """Tests for ``Annotated[T, Description(...)]`` marker."""

    def test_unwrap_annotated_with_description(self):
        from aster.metadata import unwrap_annotated, Description
        from typing import Annotated

        inner, marker = unwrap_annotated(
            Annotated[str, Description("Name to greet.", tags=("pii",))]
        )
        assert inner is str
        assert marker is not None
        assert marker.text == "Name to greet."
        assert marker.tags == ("pii",)

    def test_unwrap_annotated_without_description(self):
        from aster.metadata import unwrap_annotated
        from typing import Annotated

        inner, marker = unwrap_annotated(Annotated[str, "not a Description"])
        assert inner is str
        assert marker is None

    def test_unwrap_plain_type(self):
        from aster.metadata import unwrap_annotated

        inner, marker = unwrap_annotated(str)
        assert inner is str
        assert marker is None


class TestContractIdStabilityUnderTagChanges:
    """Tags must NOT change contract_id (regression guard for the
    non-canonical metadata invariant)."""

    def test_service_tags_do_not_change_contract_id(self):
        from aster.contract.identity import (
            ServiceContract, canonical_xlang_bytes, compute_contract_id,
        )

        @wire_type("tags_test/Req")
        @dataclass
        class Req:
            value: str = ""

        @wire_type("tags_test/Resp")
        @dataclass
        class Resp:
            result: str = ""

        @service(name="Tagged", version=1, metadata=Metadata(tags=["readonly"]))
        class TaggedSvc:
            @rpc(metadata=Metadata(tags=["cheap"]))
            async def do_it(self, req: Req) -> Resp: ...

        @service(name="Tagged", version=1, metadata=Metadata(tags=["experimental", "destructive"]))
        class RetaggedSvc:
            @rpc(metadata=Metadata(tags=["slow"]))
            async def do_it(self, req: Req) -> Resp: ...

        c1 = ServiceContract.from_service_info(TaggedSvc.__aster_service_info__)
        c2 = ServiceContract.from_service_info(RetaggedSvc.__aster_service_info__)

        id1 = compute_contract_id(canonical_xlang_bytes(c1))
        id2 = compute_contract_id(canonical_xlang_bytes(c2))

        assert id1 == id2, (
            f"Tag changes altered contract_id -- "
            f"{id1[:16]}… vs {id2[:16]}…"
        )

    def test_field_description_and_tags_do_not_change_contract_id(self):
        from aster import describe
        from aster.contract.identity import (
            ServiceContract, canonical_xlang_bytes, compute_contract_id,
        )

        @wire_type("tags_field_test/ReqPlain")
        @dataclass
        class ReqPlain:
            name: str = ""

        @wire_type("tags_field_test/ReqDescribed")
        @dataclass
        class ReqDescribed:
            name: str = describe("Name of the user.", tags=["pii"])

        @wire_type("tags_field_test/Resp")
        @dataclass
        class Resp:
            ok: bool = False

        @service(name="FieldTag", version=1)
        class PlainSvc:
            @rpc
            async def act(self, req: ReqPlain) -> Resp: ...

        @service(name="FieldTag", version=1)
        class DescribedSvc:
            @rpc
            async def act(self, req: ReqDescribed) -> Resp: ...

        c1 = ServiceContract.from_service_info(PlainSvc.__aster_service_info__)
        c2 = ServiceContract.from_service_info(DescribedSvc.__aster_service_info__)

        id1 = compute_contract_id(canonical_xlang_bytes(c1))
        id2 = compute_contract_id(canonical_xlang_bytes(c2))

        assert id1 == id2, (
            f"Field description/tags altered contract_id -- "
            f"{id1[:16]}… vs {id2[:16]}…"
        )


class TestManifestPropagation:
    """Descriptions and tags flow through extract_method_descriptors into the
    per-method / per-field dicts that MCP and the shell consume."""

    def test_method_description_and_tags_flow_to_manifest(self):
        from aster.contract.manifest import extract_method_descriptors

        @service(name="Mfest", version=1,
                 metadata=Metadata(description="Top level.", tags=["readonly"]))
        class MfestSvc:
            @rpc(metadata=Metadata(description="Do it.", tags=["cheap", "readonly"]))
            async def do_it(self, req: _PlainNoDesc) -> _RespNoDesc: ...

            @rpc(metadata=Metadata(description="Old method.", deprecated=True))
            async def legacy(self, req: _PlainNoDesc) -> _RespNoDesc: ...

        svc_info = MfestSvc.__aster_service_info__
        methods = {m["name"]: m for m in extract_method_descriptors(svc_info)}

        assert methods["do_it"]["description"] == "Do it."
        assert methods["do_it"]["tags"] == ["cheap", "readonly"]
        assert methods["do_it"]["deprecated"] is False

        assert methods["legacy"]["description"] == "Old method."
        assert methods["legacy"]["deprecated"] is True

    def test_field_description_and_tags_flow_via_describe(self):
        from aster.contract.manifest import extract_method_descriptors

        @service(name="DescSvc", version=1)
        class DescSvc:
            @rpc
            async def greet(self, req: _ReqDesc) -> _RespDesc: ...

        svc_info = DescSvc.__aster_service_info__
        methods = extract_method_descriptors(svc_info)
        (m,) = methods
        by_name = {f["name"]: f for f in m["fields"]}
        assert by_name["name"]["description"] == "Name of user."
        assert by_name["name"]["tags"] == ["pii"]
        assert by_name["api_key"]["description"] == "API key."
        assert by_name["api_key"]["tags"] == ["secret"]
        assert by_name["plain"]["description"] == ""
        assert by_name["plain"]["tags"] == []

        resp_by_name = {f["name"]: f for f in m["response_fields"]}
        assert resp_by_name["greeting"]["description"] == "Response greeting."

    def test_field_description_flows_via_wire_type_metadata(self):
        """The alternate authoring path (``@wire_type(metadata={...})``) also
        reaches the manifest field dict."""
        from aster.contract.manifest import extract_method_descriptors

        @service(name="WtMetaSvc", version=1)
        class WtMetaSvc:
            @rpc
            async def pay(self, req: _ReqWtMeta) -> _RespWtMeta: ...

        methods = extract_method_descriptors(WtMetaSvc.__aster_service_info__)
        (m,) = methods
        amount_f = next(f for f in m["fields"] if f["name"] == "amount")
        assert amount_f["description"] == "In cents."
        assert amount_f["tags"] == ["money"]

    def test_inline_param_description_flows_to_manifest(self):
        from typing import Annotated
        from aster.contract.manifest import extract_method_descriptors

        @service(name="InlineSvc", version=1)
        class InlineSvc:
            @rpc
            async def greet(
                self,
                name: Annotated[str, Description("Name to greet.", tags=("pii",))],
                locale: Annotated[str, Description("BCP 47 locale.")] = "en-US",
            ) -> _InlineGreeting: ...

        methods = extract_method_descriptors(InlineSvc.__aster_service_info__)
        (m,) = methods
        assert m["request_style"] == "inline"
        by_name = {f["name"]: f for f in m["fields"]}
        assert by_name["name"]["description"] == "Name to greet."
        assert by_name["name"]["tags"] == ["pii"]
        assert by_name["locale"]["description"] == "BCP 47 locale."

    def test_service_description_flows_to_contract_manifest(self):
        from aster.contract.identity import (
            ServiceContract, build_type_graph, resolve_with_cycles,
        )
        from aster.contract.publication import build_collection

        @service(name="DocSvcMfest", version=1,
                 metadata=Metadata(description="Doc service.", tags=["readonly"]))
        class DocSvcMfest:
            @rpc
            async def do_it(self, req: _PlainNoDesc) -> _RespNoDesc: ...

        svc_info = DocSvcMfest.__aster_service_info__
        roots: list[type] = []
        for m in svc_info.methods.values():
            if isinstance(m.request_type, type):
                roots.append(m.request_type)
            if isinstance(m.response_type, type):
                roots.append(m.response_type)
        types = build_type_graph(roots)
        type_defs = resolve_with_cycles(types)
        contract = ServiceContract.from_service_info(svc_info)

        entries = build_collection(contract, type_defs, service_info=svc_info)
        by_name = dict(entries)

        import json
        manifest = json.loads(by_name["manifest.json"])
        assert manifest["description"] == "Doc service."
        assert manifest["tags"] == ["readonly"]


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
