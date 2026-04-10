"""
aster_cli.shell.commands -- Built-in shell commands.

Each command is a plugin: usable interactively and (where applicable)
as ``aster <noun> <verb>`` from the command line.
"""

from __future__ import annotations

import asyncio
import json
from typing import Any

from aster_cli.access import (
    cmd_access_delegation,
    cmd_access_grant,
    cmd_access_list,
    cmd_access_public_private,
    cmd_access_revoke,
)
from aster_cli.join import cmd_join, cmd_status, cmd_verify
from aster_cli.publish import cmd_discover, cmd_publish, cmd_set_visibility, cmd_unpublish, cmd_update_service
from aster_cli.shell.plugin import (
    Argument,
    CommandContext,
    ShellCommand,
    get_commands_for_path,
    register,
)
import argparse
import re
import shlex
import time

from rich.progress import Progress

from aster_cli.shell.hooks import get_hook_registry
from aster_cli.shell.invoker import invoke_method
from aster_cli.shell.vfs import (
    NodeKind,
    VfsNode,
    ensure_directory_handle,
    ensure_loaded,
    resolve_path,
)


# ── Navigation ────────────────────────────────────────────────────────────────


@register
class CdCommand(ShellCommand):
    name = "cd"
    description = "Change directory"
    contexts = []  # global

    async def execute(self, args: list[str], ctx: CommandContext) -> None:
        target = args[0] if args else "/"
        node, path = resolve_path(ctx.vfs_root, ctx.vfs_cwd, target)

        if node is None and path.startswith("/aster/@"):
            node = await ensure_directory_handle(ctx.vfs_root, path.split("/")[-1], ctx.connection)

        if node is None:
            ctx.display.error(f"no such path: {path}")
            return

        if node.kind in (NodeKind.BLOB, NodeKind.METHOD, NodeKind.README, NodeKind.DOC_ENTRY):
            ctx.display.error(f"{path} is not a directory")
            return
        # Collections are cd-able -- they have children (entries)

        # Lazy-load children
        await ensure_loaded(node, ctx.connection)

        # Update cwd -- the caller reads ctx.vfs_cwd after execute
        ctx.vfs_cwd = path  # type: ignore[misc]

    def get_completions(self, ctx: CommandContext, partial: str) -> list[str]:
        return _complete_path(ctx, partial, dirs_only=True)


@register
class LsCommand(ShellCommand):
    name = "ls"
    description = "List contents of the current or specified path"
    contexts = []  # global
    cli_noun_verb = None  # ls is context-dependent -- mapped per noun below

    async def execute(self, args: list[str], ctx: CommandContext) -> None:
        target = args[0] if args else "."
        node, path = resolve_path(ctx.vfs_root, ctx.vfs_cwd, target)

        if node is None:
            ctx.display.error(f"no such path: {path}")
            return

        await ensure_loaded(node, ctx.connection)

        if node.kind == NodeKind.ROOT:
            entries = [
                {"name": c.name, "kind": "dir", "detail": _kind_detail(c)}
                for c in node.sorted_children()
            ]
            ctx.display.directory_listing(entries)

        elif node.kind == NodeKind.SERVICES:
            services = []
            for c in node.sorted_children():
                await ensure_loaded(c, ctx.connection)
                services.append({
                    "name": c.name,
                    "method_count": len(c.children),
                    "version": c.metadata.get("version", 1),
                    "scoped": c.metadata.get("scoped", "shared"),
                })
            ctx.display.service_table(services)

        elif node.kind == NodeKind.SERVICE:
            methods = []
            for c in node.sorted_children():
                methods.append({
                    "name": c.name,
                    "pattern": c.metadata.get("pattern", "unary"),
                    "signature": _method_signature(c.metadata),
                    "timeout": c.metadata.get("timeout"),
                })
            ctx.display.method_table(methods, node.name)

        elif node.kind == NodeKind.BLOBS:
            blobs = []
            for c in node.sorted_children():
                blobs.append({
                    "hash": c.metadata.get("hash", c.name),
                    "size": c.metadata.get("size", "?"),
                    "tag": c.metadata.get("tag", ""),
                    "source": c.metadata.get("source", ""),
                    "is_collection": c.kind == NodeKind.COLLECTION,
                })
            ctx.display.blob_table(blobs)

        elif node.kind == NodeKind.COLLECTION:
            await ensure_loaded(node, ctx.connection)
            entries = []
            for c in node.sorted_children():
                entries.append({
                    "name": c.name,
                    "hash": c.metadata.get("hash", "?"),
                    "size": c.metadata.get("size", 0),
                })
            ctx.display.collection_entry_table(entries)

        elif node.kind == NodeKind.DOCS:
            entries = []
            for c in node.sorted_children():
                entries.append(c.metadata)
            ctx.display.doc_entry_table(entries)

        elif node.kind == NodeKind.GOSSIP:
            ctx.display.info("Gossip topics -- use 'tail' to listen to the producer mesh")
            ctx.display.print("  [cyan]mesh[/cyan]    [dim]producer mesh topic (derived from root key + salt)[/dim]")

        elif node.kind == NodeKind.ASTER:
            handles = []
            for c in node.sorted_children():
                await ensure_loaded(c, ctx.connection)
                # Count non-README children as services
                svc_count = sum(
                    1 for ch in c.children.values() if ch.kind == NodeKind.SERVICE
                )
                handles.append({
                    "name": c.name,
                    "registered": c.metadata.get("registered", True),
                    "service_count": svc_count,
                    "description": "",
                })
            ctx.display.handle_listing(handles)

        elif node.kind == NodeKind.HANDLE:
            services = []
            for c in node.sorted_children():
                if c.kind == NodeKind.README:
                    continue
                await ensure_loaded(c, ctx.connection)
                published = c.metadata.get("published", True)
                services.append({
                    "display_name": c.name,
                    "name": c.metadata.get("name", c.name),
                    "published": published,
                    "method_count": len([ch for ch in c.children.values() if ch.kind == NodeKind.METHOD]),
                    "version": c.metadata.get("version", 1),
                    "endpoints": c.metadata.get("endpoints", 0),
                    "description": c.metadata.get("description", ""),
                })
            # Show README hint if present
            readme = node.child("README.md")
            if readme:
                ctx.display.info("README.md available -- use: cat README.md")
            ctx.display.handle_service_listing(services, node.name)

        elif node.kind == NodeKind.README:
            content = node.metadata.get("content", "")
            ctx.display.readme_content(content)

        elif node.kind == NodeKind.COLLECTION:
            await ensure_loaded(node, ctx.connection)
            entries = []
            for c in node.sorted_children():
                entries.append({
                    "name": c.name,
                    "hash": c.metadata.get("hash", "?"),
                    "size": c.metadata.get("size", 0),
                })
            ctx.display.collection_entry_table(entries)

        elif node.kind in (NodeKind.BLOB, NodeKind.METHOD, NodeKind.DOC_ENTRY):
            ctx.display.json_value(node.metadata)

        else:
            entries = [
                {"name": c.name, "kind": "dir" if c.children or not c.loaded else "file"}
                for c in node.sorted_children()
            ]
            ctx.display.directory_listing(entries)

    def get_completions(self, ctx: CommandContext, partial: str) -> list[str]:
        return _complete_path(ctx, partial, dirs_only=False)


# ── Introspection ─────────────────────────────────────────────────────────────


@register
class DescribeCommand(ShellCommand):
    name = "describe"
    description = "Show detailed contract info for a service"
    contexts = ["/services", "/services/*", "/aster/*/*"]
    cli_noun_verb = ("service", "describe")

    def get_arguments(self) -> list[Argument]:
        return [Argument(name="service", description="Service name", positional=True)]

    async def execute(self, args: list[str], ctx: CommandContext) -> None:
        # Determine which service to describe
        node, path = resolve_path(ctx.vfs_root, ctx.vfs_cwd, ".")
        if node and node.kind == NodeKind.SERVICE:
            service_name = node.name
        elif args:
            service_name = args[0]
        else:
            ctx.display.error("usage: describe <service>")
            return

        # Fetch contract details
        try:
            if node and node.kind == NodeKind.SERVICE and node.metadata.get("contract"):
                contract = node.metadata.get("contract")
            else:
                contract = await ctx.connection.get_contract(service_name)
            if contract is None:
                ctx.display.error(f"no contract found for {service_name}")
                return
            ctx.display.contract_tree(
                contract if isinstance(contract, dict) else _contract_to_dict(contract)
            )
        except Exception as e:
            ctx.display.error(f"failed to get contract: {e}")

    def get_completions(self, ctx: CommandContext, partial: str) -> list[str]:
        services_node = ctx.vfs_root.child("services")
        if services_node is None:
            return []
        return [c.name for c in services_node.sorted_children()
                if c.name.lower().startswith(partial.lower())]


@register
class CatCommand(ShellCommand):
    name = "cat"
    description = "Display file or blob content"
    contexts = []  # global -- works in blobs and directory handle contexts
    cli_noun_verb = ("blob", "cat")

    def get_arguments(self) -> list[Argument]:
        return [Argument(name="target", description="File name or blob hash", positional=True)]

    async def execute(self, args: list[str], ctx: CommandContext) -> None:
        # Resolve target -- either from arg or current directory
        if args:
            node, path = resolve_path(ctx.vfs_root, ctx.vfs_cwd, args[0])
        else:
            node, path = resolve_path(ctx.vfs_root, ctx.vfs_cwd, ".")

        if node is None and not args:
            ctx.display.error("usage: cat <target>")
            return

        # Handle known node types
        if node and node.kind == NodeKind.README:
            content = node.metadata.get("content", "")
            ctx.display.readme_content(content)
            return

        if node and node.kind == NodeKind.DOC_ENTRY:
            key = node.metadata.get("key", node.name)
            try:
                content = await ctx.connection.read_doc_entry(key)
                if content is None:
                    ctx.display.error(f"no content for doc entry: {key}")
                    return
                _display_bytes(ctx.display, content)
            except Exception as e:
                ctx.display.error(f"failed to read doc entry: {e}")
            return

        if node and node.kind == NodeKind.COLLECTION:
            # Show collection entries instead of raw binary
            await ensure_loaded(node, ctx.connection)
            entries = []
            for c in node.sorted_children():
                entries.append({
                    "name": c.name,
                    "hash": c.metadata.get("hash", "?"),
                    "size": c.metadata.get("size", 0),
                })
            ctx.display.collection_entry_table(entries)
            return

        # Resolve blob hash -- prefer full hash from VFS metadata
        if node and node.kind == NodeKind.BLOB:
            blob_hash = node.metadata.get("hash", node.name)
        elif args:
            blob_hash = args[0]
        else:
            ctx.display.error("usage: cat <target>")
            return

        try:
            content = await ctx.connection.read_blob(blob_hash)
            _display_bytes(ctx.display, content)
        except Exception as e:
            ctx.display.error(f"failed to read: {e}")

    def get_completions(self, ctx: CommandContext, partial: str) -> list[str]:
        # Complete file names in current dir
        node, _ = resolve_path(ctx.vfs_root, ctx.vfs_cwd, ".")
        if node is None:
            return []
        return [c.name for c in node.sorted_children()
                if c.name.lower().startswith(partial.lower())]


@register
class SaveCommand(ShellCommand):
    name = "save"
    description = "Download a blob to a local file"
    contexts = ["/blobs", "/blobs/*"]
    cli_noun_verb = ("blob", "save")

    def get_arguments(self) -> list[Argument]:
        return [
            Argument(name="hash", description="Blob hash (or prefix)", positional=True, required=True),
            Argument(name="path", description="Local file path", positional=True, required=True),
        ]

    async def execute(self, args: list[str], ctx: CommandContext) -> None:
        if len(args) < 2:
            # If at a blob node, only need the output path
            node, _ = resolve_path(ctx.vfs_root, ctx.vfs_cwd, ".")
            if node and node.kind == NodeKind.BLOB and len(args) == 1:
                blob_hash = node.metadata.get("hash", node.name)
                out_path = args[0]
            else:
                ctx.display.error("usage: save <hash> <path>")
                return
        else:
            blob_hash = args[0]
            out_path = args[1]

        try:
            with Progress(console=ctx.display.console) as progress:
                task = progress.add_task(f"Downloading {blob_hash[:12]}…", total=None)
                content = await ctx.connection.read_blob(blob_hash)
                progress.update(task, total=len(content), completed=len(content))

            with open(out_path, "wb") as f:
                f.write(content if isinstance(content, bytes) else content.encode())

            ctx.display.success(f"Saved to {out_path} ({len(content)} bytes)")
        except Exception as e:
            ctx.display.error(f"failed to save blob: {e}")


# ── Service invocation ────────────────────────────────────────────────────────


@register
class InvokeCommand(ShellCommand):
    name = "invoke"
    description = "Invoke an RPC method"
    contexts = ["/services/*", "/aster/*/*"]
    cli_noun_verb = ("service", "invoke")

    def get_arguments(self) -> list[Argument]:
        return [
            Argument(name="method", description="Method name", positional=True, required=True),
            Argument(name="args", description="JSON arguments"),
        ]

    async def execute(self, args: list[str], ctx: CommandContext) -> None:
        if not args:
            ctx.display.error("usage: invoke <method> [key=value ...] or <method> '{json}'")
            return

        # If we're calling from /services/<name>, resolve service name
        node, _ = resolve_path(ctx.vfs_root, ctx.vfs_cwd, ".")
        if node and node.kind == NodeKind.SERVICE:
            service_name = node.name
            method_name = args[0]
            call_args = args[1:]
        elif "/" in args[0] or "." in args[0]:
            # service.method or service/method syntax
            parts = args[0].replace("/", ".").split(".", 1)
            service_name = parts[0]
            method_name = parts[1] if len(parts) > 1 else ""
            call_args = args[1:]
        else:
            ctx.display.error("navigate to a service first, or use service.method syntax")
            return

        # Parse arguments
        payload = _parse_call_args(call_args)

        await invoke_method(ctx, service_name, method_name, payload)

    def get_completions(self, ctx: CommandContext, partial: str) -> list[str]:
        node, _ = resolve_path(ctx.vfs_root, ctx.vfs_cwd, ".")
        if node and node.kind == NodeKind.SERVICE:
            return [c.name for c in node.sorted_children()
                    if c.name.lower().startswith(partial.lower())]
        return []


# ── Direct method invocation (./methodName syntax) ───────────────────────────


@register
class DirectInvokeCommand(ShellCommand):
    """Hidden command that handles ./methodName syntax."""

    name = "./"
    description = "Direct method invocation"
    contexts = ["/services/*", "/aster/*/*"]
    hidden = True

    async def execute(self, args: list[str], ctx: CommandContext) -> None:
        # args[0] is the full "./methodName" or "methodName"
        if not args:
            return
        method_name = args[0].lstrip("./")
        call_args = args[1:]

        node, _ = resolve_path(ctx.vfs_root, ctx.vfs_cwd, ".")
        if not node or node.kind != NodeKind.SERVICE:
            ctx.display.error("direct invocation requires being in a service directory")
            return

        # Check if the method's schema is CLI-compatible when using key=value
        m_node = node.child(method_name)
        if m_node and call_args:
            fields = m_node.metadata.get("fields", [])
            hint = _check_cli_compatible(fields)
            if hint:
                ctx.display.warning(hint)

        payload = _parse_call_args(call_args)
        await invoke_method(ctx, node.name, method_name, payload)


# ── Shell utilities ───────────────────────────────────────────────────────────


@register
class PwdCommand(ShellCommand):
    name = "pwd"
    description = "Print current path"
    contexts = []

    async def execute(self, args: list[str], ctx: CommandContext) -> None:
        ctx.display.print(ctx.vfs_cwd)


@register
class HelpCommand(ShellCommand):
    name = "help"
    description = "Show available commands"
    contexts = []

    async def execute(self, args: list[str], ctx: CommandContext) -> None:

        commands = get_commands_for_path(ctx.vfs_cwd)
        ctx.display.print("[bold]Available commands:[/bold]")
        for cmd in sorted(commands, key=lambda c: c.name):
            ctx.display.print(f"  [green]{cmd.name:16s}[/green] {cmd.description}")

        # Show methods as direct invocations if in a service
        node, _ = resolve_path(ctx.vfs_root, ctx.vfs_cwd, ".")
        if node and node.kind == NodeKind.SERVICE and node.children:
            ctx.display.print()
            ctx.display.print("[bold]Direct invocation:[/bold]")
            for child in node.sorted_children():
                sig = _method_signature(child.metadata)
                ctx.display.print(f"  [green]./{child.name:14s}[/green] {sig}")


@register
class ExitCommand(ShellCommand):
    name = "exit"
    description = "Exit the shell"
    contexts = []

    async def execute(self, args: list[str], ctx: CommandContext) -> None:
        raise SystemExit(0)


@register
class RefreshCommand(ShellCommand):
    name = "refresh"
    description = "Re-fetch data from the peer"
    contexts = []

    async def execute(self, args: list[str], ctx: CommandContext) -> None:
        # Reset loaded flags so next ls/cd will re-fetch
        _reset_loaded(ctx.vfs_root)
        ctx.display.success("Cache cleared -- next listing will fetch fresh data")


@register
class JoinShellCommand(ShellCommand):
    name = "join"
    description = "Claim an Aster handle"
    contexts = []

    async def execute(self, args: list[str], ctx: CommandContext) -> None:


        parsed = argparse.Namespace(
            command="join",
            handle=args[0] if args else None,
            email=args[1] if len(args) > 1 else None,
            announcements=False,
            demo=not hasattr(ctx.connection, "_peer_addr"),
            aster=getattr(ctx.connection, "_peer_addr", None),
            root_key=None,
        )
        cmd_join(parsed)
        _after_mutation(ctx)


@register
class VerifyShellCommand(ShellCommand):
    name = "verify"
    description = "Verify a pending handle claim"
    contexts = []

    async def execute(self, args: list[str], ctx: CommandContext) -> None:


        parsed = argparse.Namespace(
            command="verify",
            code=args[0] if args else None,
            resend="--resend" in args,
            demo=not hasattr(ctx.connection, "_peer_addr"),
            aster=getattr(ctx.connection, "_peer_addr", None),
            root_key=None,
        )
        cmd_verify(parsed)
        _after_mutation(ctx)


@register
class WhoamiShellCommand(ShellCommand):
    name = "whoami"
    description = "Show local identity state"
    contexts = []

    async def execute(self, args: list[str], ctx: CommandContext) -> None:


        cmd_status(argparse.Namespace(command="whoami", raw_json=False, local_only=False, aster=getattr(ctx.connection, "_peer_addr", None), root_key=None))


@register
class StatusShellCommand(ShellCommand):
    name = "status"
    description = "Alias for whoami"
    contexts = []

    async def execute(self, args: list[str], ctx: CommandContext) -> None:


        cmd_status(argparse.Namespace(command="status", raw_json=False, local_only=False, aster=getattr(ctx.connection, "_peer_addr", None), root_key=None))


@register
class PublishShellCommand(ShellCommand):
    name = "publish"
    description = "Publish a service to @aster"
    contexts = ["/services", "/services/*", "/aster/*", "/aster/*/*"]

    async def execute(self, args: list[str], ctx: CommandContext) -> None:


        node, _ = resolve_path(ctx.vfs_root, ctx.vfs_cwd, ".")
        target = args[0] if args else (node.metadata.get("name") if node and node.kind == NodeKind.SERVICE else None)
        if not target:
            ctx.display.error("usage: publish <MODULE:CLASS|service>")
            return
        cmd_publish(
            argparse.Namespace(
                command="publish",
                target=target,
                manifest=".aster/manifest.json",
                semver=None,
                aster=getattr(ctx.connection, "_peer_addr", None),
                root_key=None,
                identity_file=None,
                endpoint_id=None,
                relay="",
                endpoint_ttl="5m",
                description=node.metadata.get("description", "") if node and node.kind == NodeKind.SERVICE else "",
                status=node.metadata.get("status", "experimental") if node and node.kind == NodeKind.SERVICE else "experimental",
                public=True,
                private=False,
                open=True,
                closed=False,
                token_ttl="5m",
                rate_limit=None,
                role=[],
                demo=not hasattr(ctx.connection, "_peer_addr"),
            )
        )
        _after_mutation(ctx)


@register
class UnpublishShellCommand(ShellCommand):
    name = "unpublish"
    description = "Unpublish a service"
    contexts = ["/services", "/services/*", "/aster/*", "/aster/*/*"]

    async def execute(self, args: list[str], ctx: CommandContext) -> None:


        node, _ = resolve_path(ctx.vfs_root, ctx.vfs_cwd, ".")
        service = args[0] if args else (node.metadata.get("name") if node and node.kind == NodeKind.SERVICE else None)
        if not service:
            ctx.display.error("usage: unpublish <service>")
            return
        cmd_unpublish(
            argparse.Namespace(
                command="unpublish",
                service=service,
                aster=getattr(ctx.connection, "_peer_addr", None),
                root_key=None,
                demo=not hasattr(ctx.connection, "_peer_addr"),
            )
        )
        _after_mutation(ctx)


@register
class DiscoverShellCommand(ShellCommand):
    name = "discover"
    description = "Search published services on @aster"
    contexts = []

    async def execute(self, args: list[str], ctx: CommandContext) -> None:


        cmd_discover(
            argparse.Namespace(
                command="discover",
                query=args[0] if args else "",
                aster=getattr(ctx.connection, "_peer_addr", None),
                limit=20,
                offset=0,
                raw_json=False,
            )
        )


@register
class AccessListShellCommand(ShellCommand):
    name = "access"
    description = "List access grants for a published service"
    contexts = ["/services/*", "/aster/*/*"]

    async def execute(self, args: list[str], ctx: CommandContext) -> None:


        node, _ = resolve_path(ctx.vfs_root, ctx.vfs_cwd, ".")
        service = args[0] if args else (node.metadata.get("name") if node and node.kind == NodeKind.SERVICE else None)
        if not service:
            ctx.display.error("usage: access <service>")
            return
        cmd_access_list(
            argparse.Namespace(
                access_command="list",
                service=service,
                aster=getattr(ctx.connection, "_peer_addr", None),
                root_key=None,
                raw_json=False,
            )
        )


@register
class GrantShellCommand(ShellCommand):
    name = "grant"
    description = "Grant a consumer access to a service"
    contexts = ["/services/*", "/aster/*/*"]

    async def execute(self, args: list[str], ctx: CommandContext) -> None:


        node, _ = resolve_path(ctx.vfs_root, ctx.vfs_cwd, ".")
        service = node.metadata.get("name") if node and node.kind == NodeKind.SERVICE else None
        if len(args) < 1:
            ctx.display.error("usage: grant <consumer> [service]")
            return
        consumer = args[0]
        if len(args) > 1:
            service = args[1]
        if not service:
            ctx.display.error("usage: grant <consumer> <service>")
            return
        cmd_access_grant(
            argparse.Namespace(
                access_command="grant",
                service=service,
                consumer=consumer,
                role="consumer",
                scope="handle",
                scope_node_id=None,
                aster=getattr(ctx.connection, "_peer_addr", None),
                root_key=None,
            )
        )
        _after_mutation(ctx)


@register
class RevokeShellCommand(ShellCommand):
    name = "revoke"
    description = "Revoke a consumer's access to a service"
    contexts = ["/services/*", "/aster/*/*"]

    async def execute(self, args: list[str], ctx: CommandContext) -> None:


        node, _ = resolve_path(ctx.vfs_root, ctx.vfs_cwd, ".")
        service = node.metadata.get("name") if node and node.kind == NodeKind.SERVICE else None
        if len(args) < 1:
            ctx.display.error("usage: revoke <consumer> [service]")
            return
        consumer = args[0]
        if len(args) > 1:
            service = args[1]
        if not service:
            ctx.display.error("usage: revoke <consumer> <service>")
            return
        cmd_access_revoke(
            argparse.Namespace(
                access_command="revoke",
                service=service,
                consumer=consumer,
                aster=getattr(ctx.connection, "_peer_addr", None),
                root_key=None,
            )
        )
        _after_mutation(ctx)


@register
class VisibilityShellCommand(ShellCommand):
    name = "visibility"
    description = "Change visibility for a published service"
    contexts = ["/services/*", "/aster/*/*"]

    async def execute(self, args: list[str], ctx: CommandContext) -> None:


        node, _ = resolve_path(ctx.vfs_root, ctx.vfs_cwd, ".")
        service = node.metadata.get("name") if node and node.kind == NodeKind.SERVICE else None
        if not args:
            ctx.display.error("usage: visibility <public|private> [service]")
            return
        visibility = args[0]
        if len(args) > 1:
            service = args[1]
        if visibility not in {"public", "private"} or not service:
            ctx.display.error("usage: visibility <public|private> [service]")
            return
        cmd_set_visibility(
            argparse.Namespace(
                command="visibility",
                service=service,
                visibility=visibility,
                aster=getattr(ctx.connection, "_peer_addr", None),
                root_key=None,
            )
        )
        _after_mutation(ctx)


@register
class UpdateServiceShellCommand(ShellCommand):
    name = "update-service"
    description = "Update published service metadata"
    contexts = ["/services/*", "/aster/*/*"]

    async def execute(self, args: list[str], ctx: CommandContext) -> None:


        node, _ = resolve_path(ctx.vfs_root, ctx.vfs_cwd, ".")
        service = node.metadata.get("name") if node and node.kind == NodeKind.SERVICE else None
        if not service:
            ctx.display.error("usage: update-service [service] [--description ...] [--status ...] [--replacement ...]")
            return
        description = None
        status = None
        replacement = None
        remaining: list[str] = []
        i = 0
        while i < len(args):
            if args[i] == "--description" and i + 1 < len(args):
                description = args[i + 1]
                i += 2
            elif args[i] == "--status" and i + 1 < len(args):
                status = args[i + 1]
                i += 2
            elif args[i] == "--replacement" and i + 1 < len(args):
                replacement = args[i + 1]
                i += 2
            else:
                remaining.append(args[i])
                i += 1
        if remaining:
            service = remaining[0]
        cmd_update_service(
            argparse.Namespace(
                command="update-service",
                service=service,
                description=description,
                status=status,
                replacement=replacement,
                aster=getattr(ctx.connection, "_peer_addr", None),
                root_key=None,
            )
        )
        _after_mutation(ctx)


@register
class DelegationShellCommand(ShellCommand):
    name = "delegation"
    description = "Update a service's delegated access mode"
    contexts = ["/services/*", "/aster/*/*"]

    async def execute(self, args: list[str], ctx: CommandContext) -> None:


        node, _ = resolve_path(ctx.vfs_root, ctx.vfs_cwd, ".")
        service = node.metadata.get("name") if node and node.kind == NodeKind.SERVICE else None
        if not service:
            ctx.display.error("usage: delegation [service] [--open|--closed]")
            return
        mode = "open"
        remaining: list[str] = []
        for arg in args:
            if arg == "--closed":
                mode = "closed"
            elif arg == "--open":
                mode = "open"
            else:
                remaining.append(arg)
        if remaining:
            service = remaining[0]
        cmd_access_delegation(
            argparse.Namespace(
                access_command="delegation",
                service=service,
                open=mode == "open",
                closed=mode == "closed",
                token_ttl="5m",
                rate_limit=None,
                role=[],
                aster=getattr(ctx.connection, "_peer_addr", None),
                root_key=None,
            )
        )
        _after_mutation(ctx)


@register
class PublicShellCommand(ShellCommand):
    name = "public"
    description = "Make a published service discoverable"
    contexts = ["/services/*", "/aster/*/*"]

    async def execute(self, args: list[str], ctx: CommandContext) -> None:


        node, _ = resolve_path(ctx.vfs_root, ctx.vfs_cwd, ".")
        service = args[0] if args else (node.metadata.get("name") if node and node.kind == NodeKind.SERVICE else None)
        if not service:
            ctx.display.error("usage: public [service]")
            return
        cmd_access_public_private(
            argparse.Namespace(
                access_command="public",
                service=service,
                aster=getattr(ctx.connection, "_peer_addr", None),
                root_key=None,
            )
        )
        _after_mutation(ctx)


@register
class PrivateShellCommand(ShellCommand):
    name = "private"
    description = "Hide a published service from discovery"
    contexts = ["/services/*", "/aster/*/*"]

    async def execute(self, args: list[str], ctx: CommandContext) -> None:


        node, _ = resolve_path(ctx.vfs_root, ctx.vfs_cwd, ".")
        service = args[0] if args else (node.metadata.get("name") if node and node.kind == NodeKind.SERVICE else None)
        if not service:
            ctx.display.error("usage: private [service]")
            return
        cmd_access_public_private(
            argparse.Namespace(
                access_command="private",
                service=service,
                aster=getattr(ctx.connection, "_peer_addr", None),
                root_key=None,
            )
        )
        _after_mutation(ctx)


@register
class GenerateClientCommand(ShellCommand):
    name = "generate-client"
    description = "Generate a typed client for a service"
    contexts = ["/services", "/services/*"]
    cli_noun_verb = ("service", "generate-client")

    def get_arguments(self) -> list[Argument]:
        return [
            Argument(name="lang", description="Target language (python, go, java, etc.)", required=True),
            Argument(name="out", description="Output directory", required=True),
            Argument(name="service", description="Service name", positional=True),
        ]

    async def execute(self, args: list[str], ctx: CommandContext) -> None:
        from aster_cli.codegen import generate_python_clients, format_usage_snippet

        # Determine which services to generate for
        node, _ = resolve_path(ctx.vfs_root, ctx.vfs_cwd, ".")
        target_service = None
        if node and node.kind == NodeKind.SERVICE:
            target_service = node.name

        # Parse args
        lang = "python"
        out = "./clients/"
        package = None
        i = 0
        while i < len(args):
            if args[i] == "--lang" and i + 1 < len(args):
                lang = args[i + 1]
                i += 2
            elif args[i] == "--out" and i + 1 < len(args):
                out = args[i + 1]
                i += 2
            elif args[i] == "--package" and i + 1 < len(args):
                package = args[i + 1]
                i += 2
            else:
                target_service = args[i]
                i += 1

        if lang != "python":
            ctx.display.error(f"Only 'python' is supported for now (got '{lang}')")
            return

        # Get manifests from connection
        manifests = ctx.connection.get_manifests()
        if not manifests:
            ctx.display.error("No manifests available -- wait for service discovery to complete")
            return

        # Filter to target service if specified
        if target_service:
            if target_service not in manifests:
                ctx.display.error(f"No manifest for '{target_service}'")
                return
            manifests = {target_service: manifests[target_service]}

        # Namespace: --package flag, peer name, or endpoint_id prefix
        namespace = package or ctx.peer_name or ctx.connection.get_peer_display()
        # Sanitize: only keep alphanumeric + underscores
        namespace = re.sub(r"[^a-zA-Z0-9_]", "_", namespace).strip("_") or "aster_client"
        source = f"{namespace}/{next(iter(manifests))}" if len(manifests) == 1 else namespace

        generated = generate_python_clients(manifests, out, namespace, source)

        ctx.display.info(f"Generated {len(generated)} files")
        for f in generated:
            ctx.display.print(f"  [dim]{f}[/dim]")

        address = getattr(ctx.connection, '_peer_addr', '')
        ctx.display.print(format_usage_snippet(out, namespace, manifests, address))


# ── Session subshell ──────────────────────────────────────────────────────────


@register
class SessionCommand(ShellCommand):
    name = "session"
    description = "Open a session subshell for a session-scoped service"
    # Scoped to /services so future paths like /aster/<handle>/ get their
    # own session command via namespace-specific dispatch (no ambiguity
    # about which handle the session belongs to).
    contexts = ["/services", "/services/*"]

    def get_arguments(self) -> list[Argument]:
        return [Argument(name="service", description="Session-scoped service name", positional=True)]

    async def execute(self, args: list[str], ctx: CommandContext) -> None:
        # Determine service
        node, _ = resolve_path(ctx.vfs_root, ctx.vfs_cwd, ".")
        if node and node.kind == NodeKind.SERVICE:
            service_name = node.name
        elif args:
            service_name = args[0]
        else:
            ctx.display.error("usage: session <service>")
            return

        # Check if session-scoped
        svc_node = ctx.vfs_root.child("services")
        if svc_node:
            target = svc_node.child(service_name)
            if target and target.metadata.get("scoped") != "session":
                ctx.display.warning(f"{service_name} is not session-scoped (scoped={target.metadata.get('scoped', 'shared')})")
                ctx.display.info("Session subshell is designed for session-scoped services.")
                ctx.display.info("Shared services don't maintain per-connection state.")
                return

        # Fire session hooks
        hooks = get_hook_registry()
        for hook in hooks.session_hooks:
            await hook.on_session_start(service_name, ctx)

        ctx.display.print()
        ctx.display.print(f"[bold cyan]Session opened: {service_name}[/bold cyan]")
        ctx.display.info("This is a dedicated session -- state persists across calls.")
        ctx.display.info("Type 'end' to close the session and return to the main shell.")
        ctx.display.print()

        # Save main shell state
        saved_cwd = ctx.vfs_cwd
        ctx.vfs_cwd = f"/services/{service_name}"

        # Subshell loop
        from prompt_toolkit import PromptSession as PS
        from prompt_toolkit.formatted_text import HTML

        sub_session = PS()

        while True:
            try:
                prompt = HTML(
                    f"<style fg='#E6C06B'>{service_name}</style>"
                    f"<style fg='#666666'>~</style> "
                )
                text = await sub_session.prompt_async(prompt)
                text = text.strip()

                if not text:
                    continue

                if text in ("end", "exit", "quit"):
                    break

                # Parse and execute within the service context
                try:
                    parts = shlex.split(text)
                except ValueError:
                    parts = text.split()

                method_name = parts[0].lstrip("./")
                call_args = parts[1:]

                # Check if it's a valid method
                target_node = ctx.vfs_root.child("services")
                if target_node:
                    svc = target_node.child(service_name)
                    if svc and svc.child(method_name):
                        payload = _parse_call_args(call_args)
                        await invoke_method(ctx, service_name, method_name, payload)
                        continue

                # Built-in subshell commands
                if method_name == "help":
                    ctx.display.print("[bold]Session commands:[/bold]")
                    ctx.display.print("  [green]end[/green]         Close this session")
                    ctx.display.print("  [green]help[/green]        Show this help")
                    ctx.display.print()
                    ctx.display.print("[bold]Available methods:[/bold]")
                    if svc_node:
                        svc = svc_node.child(service_name)
                        if svc:
                            for c in svc.sorted_children():
                                sig = c.metadata.get("request_type", "")
                                pattern = c.metadata.get("pattern", "unary")
                                ctx.display.print(f"  [green]{c.name:20s}[/green] {pattern:15s} {sig}")
                elif method_name == "ls":
                    if svc_node:
                        svc = svc_node.child(service_name)
                        if svc:
                            for c in svc.sorted_children():
                                pattern = c.metadata.get("pattern", "unary")
                                ctx.display.print(f"  [green]{c.name}[/green]  [dim]{pattern}[/dim]")
                else:
                    ctx.display.error(f"unknown method: {method_name} (try 'help')")

            except KeyboardInterrupt:
                ctx.display.print()
                continue
            except EOFError:
                break

        # Restore state
        ctx.vfs_cwd = saved_cwd

        # Fire session end hooks
        for hook in hooks.session_hooks:
            await hook.on_session_end(service_name, ctx)

        ctx.display.info(f"Session closed: {service_name}")

    def get_completions(self, ctx: CommandContext, partial: str) -> list[str]:
        services_node = ctx.vfs_root.child("services")
        if services_node is None:
            return []
        return [c.name for c in services_node.sorted_children()
                if c.metadata.get("scoped") == "session"
                and c.name.lower().startswith(partial.lower())]


# ── Gossip + docs live commands ───────────────────────────────────────────────


@register
class TailCommand(ShellCommand):
    name = "tail"
    description = "Live-stream gossip messages (Ctrl-C to stop)"
    contexts = ["/gossip"]

    async def execute(self, args: list[str], ctx: CommandContext) -> None:



        ctx.display.info("Subscribing to producer mesh gossip topic…")

        try:
            topic = await ctx.connection.subscribe_gossip()
        except Exception as e:
            ctx.display.error(f"failed to subscribe: {e}")
            return

        ctx.display.success("Subscribed -- listening for messages (Ctrl-C to stop)")
        ctx.display.print()

        try:
            while True:
                try:
                    event_type, data = await asyncio.wait_for(topic.recv(), timeout=1.0)
                except asyncio.TimeoutError:
                    continue

                ts = time.strftime("%H:%M:%S")

                if event_type == "received":
                    # Try to decode as JSON (Aster producer messages are JSON)
                    if data:
                        try:
                            text = bytes(data).decode("utf-8")
                            parsed = json.loads(text)
                            ctx.display.print(f"[dim]{ts}[/dim] [green]msg[/green] ", end="")
                            ctx.display.json_value(parsed)
                        except (UnicodeDecodeError, json.JSONDecodeError):
                            ctx.display.print(
                                f"[dim]{ts}[/dim] [green]msg[/green] "
                                f"[dim]({len(data)} bytes)[/dim] {bytes(data).hex()[:40]}…"
                            )
                    else:
                        ctx.display.print(f"[dim]{ts}[/dim] [green]msg[/green] [dim](empty)[/dim]")

                elif event_type == "neighbor_up":
                    peer_id = bytes(data).hex()[:16] if data else "?"
                    ctx.display.print(f"[dim]{ts}[/dim] [cyan]+ neighbor[/cyan] {peer_id}…")

                elif event_type == "neighbor_down":
                    peer_id = bytes(data).hex()[:16] if data else "?"
                    ctx.display.print(f"[dim]{ts}[/dim] [yellow]- neighbor[/yellow] {peer_id}…")

                elif event_type == "lagged":
                    ctx.display.print(f"[dim]{ts}[/dim] [yellow]lagged[/yellow] (missed messages)")

                else:
                    ctx.display.print(f"[dim]{ts}[/dim] [dim]{event_type}[/dim]")

        except (KeyboardInterrupt, asyncio.CancelledError):
            ctx.display.print()
            ctx.display.info("Stopped listening")
        except Exception as e:
            ctx.display.error(f"gossip error: {e}")


@register
class WatchCommand(ShellCommand):
    name = "watch"
    description = "Watch live doc events (Ctrl-C to stop)"
    contexts = ["/docs"]

    async def execute(self, args: list[str], ctx: CommandContext) -> None:



        if not hasattr(ctx.connection, '_registry_event_rx') or not ctx.connection._registry_event_rx:
            ctx.display.error("no registry doc subscription available")
            return

        rx = ctx.connection._registry_event_rx
        ctx.display.success("Watching registry doc events (Ctrl-C to stop)")
        ctx.display.print()

        try:
            while True:
                try:
                    event = await asyncio.wait_for(rx.recv(), timeout=1.0)
                except asyncio.TimeoutError:
                    continue

                if event is None:
                    ctx.display.info("subscription ended")
                    break

                ts = time.strftime("%H:%M:%S")
                kind = event.kind

                if kind in ("insert_local", "insert_remote"):
                    entry = event.entry
                    if entry:
                        key_str = bytes(entry.key).decode("utf-8", errors="replace")
                        author = entry.author_id[:12]
                        source = f" [dim]from {event.from_peer[:12]}…[/dim]" if event.from_peer else ""
                        ctx.display.print(
                            f"[dim]{ts}[/dim] [green]{kind}[/green] "
                            f"[cyan]{key_str}[/cyan] [dim]by {author}…[/dim]{source}"
                        )
                    else:
                        ctx.display.print(f"[dim]{ts}[/dim] [green]{kind}[/green]")

                elif kind == "content_ready":
                    ctx.display.print(
                        f"[dim]{ts}[/dim] [blue]content_ready[/blue] {event.hash or '?'}"
                    )

                elif kind in ("neighbor_up", "neighbor_down"):
                    color = "cyan" if kind == "neighbor_up" else "yellow"
                    peer = event.peer[:16] if event.peer else "?"
                    ctx.display.print(f"[dim]{ts}[/dim] [{color}]{kind}[/{color}] {peer}…")

                elif kind == "sync_finished":
                    peer = event.peer[:16] if event.peer else "?"
                    ctx.display.print(f"[dim]{ts}[/dim] [green]sync_finished[/green] {peer}…")

                else:
                    ctx.display.print(f"[dim]{ts}[/dim] [dim]{kind}[/dim]")

        except (KeyboardInterrupt, asyncio.CancelledError):
            ctx.display.print()
            ctx.display.info("Stopped watching")
        except Exception as e:
            ctx.display.error(f"watch error: {e}")


# ── CLI-mapped blob commands ──────────────────────────────────────────────────


@register
class BlobLsCommand(ShellCommand):
    name = "blob-ls"
    description = "List blobs (CLI: aster blob ls)"
    contexts = ["/blobs"]
    cli_noun_verb = ("blob", "ls")
    hidden = True  # hidden in interactive mode -- use ls instead

    async def execute(self, args: list[str], ctx: CommandContext) -> None:
        # Navigate to /blobs and list
        node = ctx.vfs_root.child("blobs")
        if node:
            await ensure_loaded(node, ctx.connection)
            blobs = [
                {"hash": c.metadata.get("hash", c.name), "size": c.metadata.get("size", "?")}
                for c in node.sorted_children()
            ]
            ctx.display.blob_table(blobs)


@register
class ServiceLsCommand(ShellCommand):
    name = "service-ls"
    description = "List services (CLI: aster service ls)"
    contexts = ["/services"]
    cli_noun_verb = ("service", "ls")
    hidden = True

    async def execute(self, args: list[str], ctx: CommandContext) -> None:
        node = ctx.vfs_root.child("services")
        if node:
            await ensure_loaded(node, ctx.connection)
            services = []
            for c in node.sorted_children():
                await ensure_loaded(c, ctx.connection)
                services.append({
                    "name": c.name,
                    "method_count": len(c.children),
                    "version": c.metadata.get("version", 1),
                    "scoped": c.metadata.get("scoped", "shared"),
                })
            ctx.display.service_table(services)


# ── Helpers ───────────────────────────────────────────────────────────────────


def _complete_path(ctx: CommandContext, partial: str, dirs_only: bool = False) -> list[str]:
    """Complete a VFS path from a partial string.

    Splits partial into parent directory + leaf prefix, resolves the parent,
    and filters children by the prefix.

    Examples:
        partial=""      → children of cwd
        partial="He"    → children of cwd starting with "He"
        partial="srv/H" → children of cwd/srv starting with "H"
    """
    # Split into parent path and leaf prefix
    if "/" in partial and not partial.endswith("/"):
        parent_path = partial.rsplit("/", 1)[0] or "/"
        prefix = partial.rsplit("/", 1)[1]
        path_prefix = partial.rsplit("/", 1)[0] + "/"
    elif partial.endswith("/"):
        parent_path = partial.rstrip("/") or "/"
        prefix = ""
        path_prefix = partial if partial.endswith("/") else partial + "/"
    else:
        parent_path = "."
        prefix = partial
        path_prefix = ""

    node, _path = resolve_path(ctx.vfs_root, ctx.vfs_cwd, parent_path)
    if node is None:
        return []

    prefix_lower = prefix.lower()
    results = []
    for c in node.sorted_children():
        if dirs_only and c.kind in (NodeKind.BLOB, NodeKind.METHOD, NodeKind.DOC_ENTRY):
            continue
        if c.name.lower().startswith(prefix_lower):
            suffix = "/" if c.kind not in (NodeKind.BLOB, NodeKind.METHOD, NodeKind.DOC_ENTRY) else ""
            results.append(path_prefix + c.name + suffix)
    return results


def _set_nested(d: dict[str, Any], dotted_key: str, value: Any) -> None:
    """Set a value in a nested dict using dot syntax.

    ``_set_nested(d, "a.b.c", 1)`` produces ``d["a"]["b"]["c"] = 1``.
    """
    parts = dotted_key.split(".")
    for part in parts[:-1]:
        if part not in d or not isinstance(d[part], dict):
            d[part] = {}
        d = d[part]
    d[parts[-1]] = value


def _parse_call_args(args: list[str]) -> dict[str, Any]:
    """Parse call arguments from shell tokens.

    Supports:
      - key=value pairs: ``name="World" count=5``
      - Dot syntax for nested objects: ``config.timeout=30`` produces
        ``{"config": {"timeout": 30}}``
      - Raw JSON string: ``'{"name": "World"}'``
      - Positional value for single-arg methods: ``"World"``
    """
    if not args:
        return {}

    # Try as a single JSON string
    joined = " ".join(args)
    if joined.startswith("{"):
        try:
            return json.loads(joined)
        except json.JSONDecodeError:
            pass

    # Try as key=value pairs (with dot syntax for nesting)
    result: dict[str, Any] = {}
    positional: list[str] = []

    for arg in args:
        if "=" in arg:
            key, value = arg.split("=", 1)
            value = value.strip("'\"")
            try:
                parsed = json.loads(value)
            except (json.JSONDecodeError, ValueError):
                parsed = value
            _set_nested(result, key, parsed)
        else:
            positional.append(arg.strip("'\""))

    # Leftover positional args after key=value pairs: merge into last key
    if positional and result:
        last_key = list(result.keys())[-1]
        current = str(result[last_key])
        result[last_key] = current + " " + " ".join(positional)

    # Only positional args
    if positional and not result:
        if len(positional) == 1:
            try:
                return {"_positional": json.loads(positional[0])}
            except (json.JSONDecodeError, ValueError):
                return {"_positional": positional[0]}
        for i, v in enumerate(positional):
            result[f"_arg{i}"] = v

    return result


def _check_cli_compatible(fields: list[dict[str, Any]]) -> str | None:
    """Check if a method's request schema can be built via CLI key=value syntax.

    Returns None if compatible, or a hint message for unsupported schemas.
    """
    for f in fields:
        kind = f.get("kind", "")
        name = f.get("name", "?")

        if kind == "list":
            item_kind = f.get("item_kind", "string")
            if item_kind == "ref":
                item_ref = f.get("item_ref", "object")
                return (
                    f"field '{name}' is list<{item_ref}> which can't be "
                    f"built with key=value syntax.\n"
                    f"  Use JSON: ./method '{{\"{ name}\": [...]}}'"
                )
        elif kind == "map":
            value_kind = f.get("value_kind", "string")
            if value_kind == "ref":
                return (
                    f"field '{name}' is map<{f.get('key_kind', 'string')}, "
                    f"{f.get('value_ref', 'object')}> which can't be "
                    f"built with key=value syntax.\n"
                    f"  Use JSON: ./method '{{\"{ name}\": {{...}}}}'"
                )

    return None


def _method_signature(metadata: dict[str, Any]) -> str:
    """Build a human-readable method signature from metadata."""
    req = metadata.get("request_type", "")
    resp = metadata.get("response_type", "")
    if req and resp:
        return f"({req}) ->{resp}"
    elif req:
        return f"({req}) ->…"
    elif resp:
        return f"() ->{resp}"
    return ""


def _display_bytes(display: Any, content: Any) -> None:
    """Display bytes content, trying UTF-8 text then hex."""
    if isinstance(content, bytes):
        try:
            text = content.decode("utf-8")
            # Try to pretty-print JSON
            try:
                parsed = json.loads(text)
                display.json_value(parsed)
            except (json.JSONDecodeError, ValueError):
                display.print(text)
        except UnicodeDecodeError:
            display.info(f"(binary data, {len(content)} bytes)")
            display.print(content.hex()[:200] + ("…" if len(content) > 100 else ""))
    else:
        display.print(str(content))


def _kind_detail(node: VfsNode) -> str:
    """Short detail string for a top-level directory."""
    details = {
        NodeKind.BLOBS: "content-addressed storage",
        NodeKind.SERVICES: "RPC services",
        NodeKind.DOCS: "registry documents",
        NodeKind.GOSSIP: "pub/sub topics",
        NodeKind.ASTER: "service directory",
    }
    return details.get(node.kind, "")


def _after_mutation(ctx: CommandContext) -> None:
    """Refresh shell state after local or remote mutating commands."""
    _reset_loaded(ctx.vfs_root)


def _reset_loaded(node: VfsNode) -> None:
    """Recursively clear loaded flags."""
    children = list(node.children.values())
    node.loaded = node.kind == NodeKind.ROOT  # keep root loaded
    if node.kind != NodeKind.ROOT:
        node.children.clear()
    for child in children:
        _reset_loaded(child)


def _lang_ext(lang: str) -> str:
    """File extension for a language."""
    return {
        "python": "py",
        "go": "go",
        "java": "java",
        "typescript": "ts",
        "javascript": "js",
        "csharp": "cs",
        "rust": "rs",
    }.get(lang.lower(), lang)
