"""Gateway identity: calls made on behalf of users, and ``aster.ops.NodeConfig``.

Design: ``docs/_internal/web_gateway/gateway-identity.md``. An edge holding
:data:`GATEWAY_ROLE` calls a service on behalf of a user with one JWT per
call, either the identity provider's own token (forwarded unchanged) or one
the edge mints (:meth:`aster.AsterClient.mint_user_token`)::

    token = client.mint_user_token("alice", ["notes.read"], [AMR_WEBAUTHN], ttl=60)
    notes = (await client.client(Notes)).with_options(CallOptions(on_behalf_of=token))

The server (``AsterConfig.identity_accept_on_behalf_of``) verifies the token
before Gate 3; the handler's :class:`aster.CallContext` then has the user as
``principal`` (``iss|sub``), the edge as ``actor``, how the user signed in as
``auth_methods`` (``require_auth_method``), and the whole verified identity as
``on_behalf_of``. Everything is configured through :class:`aster.AsterConfig`
(``[identity]``, ``[identity.publisher]``, ``[remote_config]``), as the Rust
``AsterServer`` / ``AsterClient`` are.

:class:`NodeConfigClient` changes a running server's name, tags, typed
attributes and ``[identity]`` override through ``aster.ops.NodeConfig``
(Rust ``NodeConfigClient``); the caller needs :data:`CONFIG_ADMIN_ROLE`.
Handlers read the attributes with ``CallContext.node_attribute``; they
describe the node and never authorise.
"""

from __future__ import annotations

from dataclasses import dataclass, field
from typing import Any, Mapping

from ._aster import (
    AMR_API_KEY,
    AMR_PASSWORD,
    AMR_WEBAUTHN,
    CONFIG_ADMIN_ROLE,
    GATEWAY_ROLE,
    IDENTITY_PUBLISHER_ROLE,
    ISSUABLE_ROLES_ATTRIBUTE,
    ON_BEHALF_OF_METADATA,
    ForwardedIdentity,
    NativeNodeConfigClient,
    bearer_token,
)
from .config import IdentityPublisherConfig, TrustedIssuer
from .interceptors.base import Interceptor

__all__ = [
    "AMR_API_KEY",
    "AMR_PASSWORD",
    "AMR_WEBAUTHN",
    "CONFIG_ADMIN_ROLE",
    "GATEWAY_ROLE",
    "IDENTITY_PUBLISHER_ROLE",
    "ISSUABLE_ROLES_ATTRIBUTE",
    "ON_BEHALF_OF_METADATA",
    "ForwardedIdentity",
    "IdentityOverride",
    "IdentityPublisherConfig",
    "NodeConfigAck",
    "NodeConfigClient",
    "NodeConfigOutstanding",
    "NodeConfigSetting",
    "NodeConfigView",
    "NodeProperties",
    "TrustedIssuer",
    "bearer_token",
]


@dataclass(frozen=True)
class NodeProperties:
    """This node's ``aster.ops.NodeConfig`` properties (Rust
    ``aster::rpc::NodeProperties``): what the root or an issuer last set.
    Attributes are ``bool`` / ``int`` / ``float`` / ``str``; they describe the
    node and never authorise (Gate 3 does not read them)."""

    generation: int = 0
    name: str = ""
    tags: tuple[str, ...] = ()
    attributes: Mapping[str, Any] = field(default_factory=dict)

    def has_tag(self, tag: str) -> bool:
        return tag in self.tags

    def attribute(self, key: str) -> Any:
        return self.attributes.get(key)

    @classmethod
    def _from_native(cls, d: Mapping[str, Any]) -> "NodeProperties":
        return cls(
            generation=int(d["generation"]),
            name=d["name"],
            tags=tuple(d["tags"]),
            attributes=dict(d["attributes"]),
        )


EMPTY_NODE_PROPERTIES = NodeProperties()


@dataclass(frozen=True)
class IdentityOverride:
    """The ``identity`` section of a NodeConfig ``set`` (Rust
    ``IdentityOverride``): each field set replaces the file value; ``None``
    keeps it."""

    accept_on_behalf_of: bool | None = None
    trusted_issuers: tuple[TrustedIssuer, ...] | None = None
    edge_token_max_lifetime_secs: int | None = None
    clock_leeway_secs: int | None = None
    max_key_set_age_secs: int | None = None

    def _to_native(self) -> dict[str, Any]:
        out: dict[str, Any] = {}
        for name in (
            "accept_on_behalf_of", "edge_token_max_lifetime_secs",
            "clock_leeway_secs", "max_key_set_age_secs",
        ):
            value = getattr(self, name)
            if value is not None:
                out[name] = value
        if self.trusted_issuers is not None:
            out["trusted_issuers"] = [
                (i if isinstance(i, TrustedIssuer) else TrustedIssuer(**i))._to_native()
                for i in self.trusted_issuers
            ]
        return out


@dataclass(frozen=True)
class NodeConfigSetting:
    """One overridable setting in a :class:`NodeConfigView`: its effective
    value and where it came from (``default`` / ``file`` / ``env`` / ``code``
    / ``override``)."""

    key: str
    value: Any
    source: str


@dataclass(frozen=True)
class NodeConfigView:
    """``aster.ops.NodeConfig`` ``get`` (Rust ``NodeConfigView``)."""

    node_id: str
    generation: int
    applied_generation: int
    change: str
    changed_by: str
    changed_at_ms: int
    name: str
    tags: tuple[str, ...]
    attributes: Mapping[str, Any]
    sections: Mapping[str, Any]
    ignored_sections: tuple[str, ...]
    settings: tuple[NodeConfigSetting, ...]
    overridable_sections: tuple[str, ...]
    locked_sections: tuple[str, ...]

    def setting(self, key: str) -> NodeConfigSetting | None:
        """One setting by ``section.field`` key, e.g.
        ``identity.accept_on_behalf_of``."""
        return next((s for s in self.settings if s.key == key), None)

    @classmethod
    def _from_native(cls, d: Mapping[str, Any]) -> "NodeConfigView":
        return cls(
            node_id=d["node_id"],
            generation=d["generation"],
            applied_generation=d["applied_generation"],
            change=d["change"],
            changed_by=d["changed_by"],
            changed_at_ms=d["changed_at_ms"],
            name=d["name"],
            tags=tuple(d["tags"]),
            attributes=dict(d["attributes"]),
            sections=dict(d["sections"]),
            ignored_sections=tuple(d["ignored_sections"]),
            settings=tuple(NodeConfigSetting(**s) for s in d["settings"]),
            overridable_sections=tuple(d["overridable_sections"]),
            locked_sections=tuple(d["locked_sections"]),
        )


@dataclass(frozen=True)
class NodeConfigOutstanding:
    """A part of a change not yet in effect."""

    part: str
    state: str
    detail: str


@dataclass(frozen=True)
class NodeConfigAck:
    """The acknowledgement of a NodeConfig ``set`` / ``clear`` / ``ack``."""

    complete: bool
    generation: int
    applied_generation: int
    outstanding: tuple[NodeConfigOutstanding, ...] = ()

    @classmethod
    def _from_native(cls, d: Mapping[str, Any]) -> "NodeConfigAck":
        return cls(
            complete=d["complete"],
            generation=d["generation"],
            applied_generation=d["applied_generation"],
            outstanding=tuple(NodeConfigOutstanding(**o) for o in d["outstanding"]),
        )


class _PublisherNotice(Interceptor):
    """Client interceptor an :class:`aster.AsterClient` running
    ``[identity.publisher]`` adds to its clients: each call's
    ``aster-on-behalf-of`` token is handed to the publisher
    (``Enrollment.notice_token``), so a provider's key rotation is published
    at once (as the Rust ``AsterClient``'s unary interceptor does)."""

    def __init__(self, enrollment: Any) -> None:
        self._enrollment = enrollment

    async def on_request(self, ctx: Any, request: object) -> object:
        for key, value in (ctx.metadata or {}).items():
            if key.lower() == ON_BEHALF_OF_METADATA:
                try:
                    self._enrollment.notice_token(value)
                except LookupError:
                    pass
        return request


class NodeConfigClient:
    """``aster.ops.NodeConfig`` on a server (Rust ``NodeConfigClient``).

    Build it from an :class:`aster.AsterClient`
    (``await client.node_config()``) or from an ``aster/1`` connection.
    Every method needs :data:`CONFIG_ADMIN_ROLE` in the caller's credential;
    a refused call raises the matching :class:`aster.RpcError` subclass.
    ``deadline`` is in seconds (``None``: none).
    """

    def __init__(self, connection: Any) -> None:
        self._native = NativeNodeConfigClient(connection)

    async def get(self, *, deadline: float | None = None) -> NodeConfigView:
        """The record, every overridable setting and its source."""
        return NodeConfigView._from_native(await self._native.get(deadline=deadline))

    async def set(
        self,
        *,
        name: str = "",
        tags: tuple[str, ...] | list[str] = (),
        attributes: Mapping[str, bool | int | float | str] | None = None,
        identity: IdentityOverride | None = None,
        sections: Mapping[str, Mapping[str, Any]] | None = None,
        expected_generation: int = 0,
        deadline: float | None = None,
    ) -> NodeConfigAck:
        """Replace the whole record: name, tags, typed attributes and section
        overrides (``identity``, or raw ``sections``). ``expected_generation``
        (non-zero) makes it conditional."""
        all_sections = dict(sections or {})
        if identity is not None:
            all_sections["identity"] = identity._to_native()
        return NodeConfigAck._from_native(
            await self._native.set(
                name=name,
                tags=list(tags),
                attributes=dict(attributes or {}),
                sections=all_sections,
                expected_generation=expected_generation,
                deadline=deadline,
            )
        )

    async def clear(
        self, *, expected_generation: int = 0, deadline: float | None = None
    ) -> NodeConfigAck:
        """Remove the record: the node runs on its file settings again."""
        return NodeConfigAck._from_native(
            await self._native.clear(expected_generation=expected_generation, deadline=deadline)
        )

    async def ack(
        self, generation: int, *, timeout: float = 0.0, deadline: float | None = None
    ) -> NodeConfigAck:
        """Wait up to ``timeout`` seconds for ``generation`` to take effect."""
        return NodeConfigAck._from_native(
            await self._native.ack(generation, timeout=timeout, deadline=deadline)
        )
