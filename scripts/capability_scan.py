#!/usr/bin/env python3
"""
capability_scan.py — Extract the public API surface from the Python bindings,
hash each capability, and generate idiom hints for target languages using
Claude Haiku.

Usage:
    # Extract capabilities (no API calls)
    python scripts/capability_scan.py extract

    # Process NEW capabilities through Haiku for idiom hints
    python scripts/capability_scan.py process

    # Show what's changed since last run
    python scripts/capability_scan.py diff

    # Full report (extract + diff + process)
    python scripts/capability_scan.py run

Files:
    docs/_internal/capabilities/extracted.yaml   — auto-generated from AST
    docs/_internal/capabilities/processed.yaml   — enriched with idioms (Haiku)
    docs/_internal/capabilities/mapping.yaml     — type/pattern mapping tables
"""

from __future__ import annotations

import ast
import hashlib
import json
import sys
import textwrap
from dataclasses import dataclass, field, asdict
from datetime import datetime, timezone
from pathlib import Path
from typing import Any

# Try to import yaml; fall back to json if not available
try:
    import yaml

    def dump_yaml(data: Any, path: Path) -> None:
        path.write_text(yaml.dump(data, default_flow_style=False, sort_keys=False, width=120))

    def load_yaml(path: Path) -> Any:
        return yaml.safe_load(path.read_text()) or {}
except ImportError:
    # Fall back to JSON
    def dump_yaml(data: Any, path: Path) -> None:
        path.write_text(json.dumps(data, indent=2, default=str))

    def load_yaml(path: Path) -> Any:
        return json.loads(path.read_text()) if path.exists() else {}


# ── Paths ────────────────────────────────────────────────────────────────────

REPO_ROOT = Path(__file__).resolve().parent.parent
PYTHON_PKG = REPO_ROOT / "bindings" / "python" / "aster"
NATIVE_STUB = REPO_ROOT / "bindings" / "python" / "aster" / "_aster.pyi"
OUTPUT_DIR = REPO_ROOT / "docs" / "_internal" / "capabilities"
EXTRACTED_FILE = OUTPUT_DIR / "extracted.yaml"
PROCESSED_FILE = OUTPUT_DIR / "processed.yaml"
MAPPING_FILE = OUTPUT_DIR / "mapping.yaml"


# ── Data model ───────────────────────────────────────────────────────────────

@dataclass
class Capability:
    id: str                          # e.g. "Server.start" or "IrohNode.memory"
    module: str                      # e.g. "aster.server" or "aster._aster"
    class_name: str | None           # None for module-level functions
    method_name: str
    params: list[dict[str, str]]     # [{"name": "x", "type": "int"}, ...]
    return_type: str
    is_async: bool
    is_static: bool
    is_classmethod: bool
    is_property: bool
    decorators: list[str]
    docstring: str
    source_hash: str                 # hash of the method body
    signature_hash: str              # hash of (class, method, params, return)
    source_code: str = ""                                     # method body (for Haiku context)
    security_flags: list[str] = field(default_factory=list)  # auto-detected security concerns


# ── AST extraction ───────────────────────────────────────────────────────────

# ── Security pattern detection ───────────────────────────────────────────────

# Patterns to detect in method source code
SECURITY_PATTERNS: list[tuple[str, str]] = [
    # (flag_name, regex_or_substring_to_match)
    ("deserializes_json", "json.loads"),
    ("deserializes_json", "json.load("),
    ("deserializes_bytes", "fromhex("),
    ("deserializes_bytes", "bytes.fromhex"),
    ("deserializes_fory", "codec.decode"),
    ("deserializes_fory", "ForyCodec"),
    ("deserializes_fory", "deserialize"),
    ("decompresses", "zstd"),
    ("decompresses", "decompress"),
    ("performs_crypto", "sign("),
    ("performs_crypto", "verify("),
    ("performs_crypto", "ed25519"),
    ("performs_crypto", "blake3"),
    ("performs_crypto", "BLAKE3"),
    ("performs_crypto", "hmac"),
    ("handles_credentials", "credential"),
    ("handles_credentials", "enrollment"),
    ("handles_credentials", "root_pubkey"),
    ("handles_credentials", "secret_key"),
    ("handles_credentials", "nonce"),
    ("network_io", "read_frame"),
    ("network_io", "write_frame"),
    ("network_io", "read_to_end"),
    ("network_io", "read_exact"),
    ("network_io", "write_all"),
    ("network_io", "send_datagram"),
    ("network_io", "recv"),
    ("network_io", "connect("),
    ("enforces_limits", "MAX_FRAME_SIZE"),
    ("enforces_limits", "MAX_DECOMPRESSED"),
    ("enforces_limits", "MAX_METADATA"),
    ("enforces_limits", "validate_hex_field"),
    ("enforces_limits", "validate_metadata"),
    ("enforces_limits", "LimitExceeded"),
    ("unsafe_patterns", "eval("),
    ("unsafe_patterns", "exec("),
    ("unsafe_patterns", "pickle"),
    ("unsafe_patterns", "__import__"),
    ("unsafe_patterns", "subprocess"),
    ("file_io", "open("),
    ("file_io", "Path("),
    ("file_io", "write_text"),
    ("file_io", "read_text"),
    ("admission_gate", "admit"),
    ("admission_gate", "Gate"),
    ("admission_gate", "allowlist"),
    ("admission_gate", "hook"),
]


def _detect_security_flags(source: str) -> list[str]:
    """Detect security-relevant patterns in method source."""
    flags = set()
    source_lower = source.lower()
    for flag, pattern in SECURITY_PATTERNS:
        if pattern.lower() in source_lower:
            flags.add(flag)
    return sorted(flags)


def _annotation_str(node: ast.expr | None) -> str:
    """Convert an AST annotation node to a string."""
    if node is None:
        return ""
    return ast.unparse(node)


def _hash_source(source: str) -> str:
    """Hash source code, ignoring leading whitespace per line."""
    normalized = "\n".join(line.strip() for line in source.splitlines())
    return hashlib.sha256(normalized.encode()).hexdigest()[:16]


def _hash_signature(class_name: str | None, method_name: str,
                    params: list[dict[str, str]], return_type: str) -> str:
    """Hash the method signature for change detection."""
    sig = json.dumps({
        "class": class_name,
        "method": method_name,
        "params": params,
        "return": return_type,
    }, sort_keys=True)
    return hashlib.sha256(sig.encode()).hexdigest()[:16]


def _extract_from_class(
    cls_node: ast.ClassDef,
    module_name: str,
    source_lines: list[str],
) -> list[Capability]:
    """Extract capabilities from a class definition."""
    caps = []

    for node in ast.iter_child_nodes(cls_node):
        if not isinstance(node, (ast.FunctionDef, ast.AsyncFunctionDef)):
            continue

        # Skip private methods (but keep dunder like __init__)
        if node.name.startswith("_") and not node.name.startswith("__"):
            continue

        # Extract decorators
        decorators = []
        is_static = False
        is_classmethod = False
        is_property = False
        for dec in node.decorator_list:
            dec_name = ast.unparse(dec)
            decorators.append(dec_name)
            if dec_name == "staticmethod":
                is_static = True
            elif dec_name == "classmethod":
                is_classmethod = True
            elif dec_name == "property":
                is_property = True

        # Extract params (skip self/cls)
        params = []
        for arg in node.args.args:
            if arg.arg in ("self", "cls"):
                continue
            params.append({
                "name": arg.arg,
                "type": _annotation_str(arg.annotation),
            })

        return_type = _annotation_str(node.returns)

        # Get source for hashing
        try:
            src = ast.get_source_segment("\n".join(source_lines), node) or ""
        except Exception:
            src = node.name

        # Docstring
        docstring = ast.get_docstring(node) or ""

        # Truncate source for Haiku context (keep under ~2000 chars to manage cost)
        source_truncated = src[:2000] + ("..." if len(src) > 2000 else "") if src else ""

        cap_id = f"{cls_node.name}.{node.name}"
        caps.append(Capability(
            id=cap_id,
            module=module_name,
            class_name=cls_node.name,
            method_name=node.name,
            params=params,
            return_type=return_type,
            is_async=isinstance(node, ast.AsyncFunctionDef),
            is_static=is_static,
            is_classmethod=is_classmethod,
            is_property=is_property,
            decorators=decorators,
            docstring=docstring.split("\n")[0] if docstring else "",
            source_code=source_truncated,
            source_hash=_hash_source(src),
            signature_hash=_hash_signature(cls_node.name, node.name, params, return_type),
            security_flags=_detect_security_flags(src),
        ))

    return caps


def _extract_module_functions(
    tree: ast.Module,
    module_name: str,
    source_lines: list[str],
) -> list[Capability]:
    """Extract module-level public functions."""
    caps = []

    for node in ast.iter_child_nodes(tree):
        if not isinstance(node, (ast.FunctionDef, ast.AsyncFunctionDef)):
            continue
        if node.name.startswith("_"):
            continue

        params = []
        for arg in node.args.args:
            if arg.arg in ("self", "cls"):
                continue
            params.append({
                "name": arg.arg,
                "type": _annotation_str(arg.annotation),
            })

        return_type = _annotation_str(node.returns)

        try:
            src = ast.get_source_segment("\n".join(source_lines), node) or ""
        except Exception:
            src = node.name

        docstring = ast.get_docstring(node) or ""

        source_truncated = src[:2000] + ("..." if len(src) > 2000 else "") if src else ""

        caps.append(Capability(
            id=node.name,
            module=module_name,
            class_name=None,
            method_name=node.name,
            params=params,
            return_type=return_type,
            is_async=isinstance(node, ast.AsyncFunctionDef),
            is_static=False,
            is_classmethod=False,
            is_property=False,
            decorators=[ast.unparse(d) for d in node.decorator_list],
            docstring=docstring.split("\n")[0] if docstring else "",
            source_code=source_truncated,
            source_hash=_hash_source(src),
            signature_hash=_hash_signature(None, node.name, params, return_type),
            security_flags=_detect_security_flags(src),
        ))

    return caps


def extract_from_file(path: Path, module_name: str) -> list[Capability]:
    """Extract all public capabilities from a Python file."""
    source = path.read_text()
    source_lines = source.splitlines()
    try:
        tree = ast.parse(source, filename=str(path))
    except SyntaxError:
        return []

    caps = []

    # Module-level functions
    caps.extend(_extract_module_functions(tree, module_name, source_lines))

    # Classes
    for node in ast.iter_child_nodes(tree):
        if isinstance(node, ast.ClassDef) and not node.name.startswith("_"):
            caps.extend(_extract_from_class(node, module_name, source_lines))

    return caps


def extract_native_runtime() -> list[Capability]:
    """Extract native binding capabilities by introspecting the live module."""
    caps = []
    try:
        import aster._aster as native
    except ImportError:
        print("  Warning: cannot import aster._aster — native bindings not extracted")
        return caps

    import inspect

    for name in dir(native):
        if name.startswith("_"):
            continue
        obj = getattr(native, name)

        # Module-level functions
        if inspect.isbuiltin(obj) or (callable(obj) and not inspect.isclass(obj)):
            sig_str = ""
            try:
                sig = inspect.signature(obj)
                sig_str = str(sig)
            except (ValueError, TypeError):
                pass

            caps.append(Capability(
                id=name,
                module="aster._aster",
                class_name=None,
                method_name=name,
                params=[],
                return_type="",
                is_async="coroutine" in str(type(obj)).lower(),
                is_static=False,
                is_classmethod=False,
                is_property=False,
                decorators=[],
                docstring=(obj.__doc__ or "").split("\n")[0],
                source_hash=_hash_source(obj.__doc__ or name),
                signature_hash=_hash_signature(None, name, [], ""),
            ))
            continue

        # Classes
        if inspect.isclass(obj):
            for method_name in sorted(dir(obj)):
                if method_name.startswith("_"):
                    continue
                method = getattr(obj, method_name, None)
                if method is None:
                    continue

                is_prop = isinstance(inspect.getattr_static(obj, method_name, None), property)

                params = []
                try:
                    sig = inspect.signature(method)
                    for pname, param in sig.parameters.items():
                        if pname in ("self", "cls"):
                            continue
                        ann = param.annotation
                        type_str = ann.__name__ if hasattr(ann, "__name__") else str(ann) if ann != inspect.Parameter.empty else ""
                        params.append({"name": pname, "type": type_str})
                except (ValueError, TypeError):
                    pass

                doc = (getattr(method, "__doc__", None) or "").split("\n")[0]
                cap_id = f"{name}.{method_name}"

                caps.append(Capability(
                    id=cap_id,
                    module="aster._aster",
                    class_name=name,
                    method_name=method_name,
                    params=params,
                    return_type="",
                    is_async="coroutine" in doc.lower() or "async" in doc.lower(),
                    is_static=isinstance(inspect.getattr_static(obj, method_name, None), staticmethod),
                    is_classmethod=isinstance(inspect.getattr_static(obj, method_name, None), classmethod),
                    is_property=is_prop,
                    decorators=[],
                    docstring=doc,
                    source_hash=_hash_source(doc or cap_id),
                    signature_hash=_hash_signature(name, method_name, params, ""),
                ))

    return caps


def extract_all() -> list[Capability]:
    """Extract capabilities from the entire Python package."""
    all_caps: list[Capability] = []

    # Native bindings (PyO3/NAPI) are NOT scanned — they come from Rust core
    # and are shared across all language bindings automatically.
    # We only scan the pure Python framework layer that must be reimplemented.

    # 2. Pure Python package
    for py_file in sorted(PYTHON_PKG.rglob("*.py")):
        if py_file.name.startswith("_") and py_file.name != "__init__.py":
            continue
        # Build module name
        rel = py_file.relative_to(PYTHON_PKG.parent)
        module = str(rel).replace("/", ".").replace(".py", "")
        if module.endswith(".__init__"):
            module = module[:-9]
        all_caps.extend(extract_from_file(py_file, module))

    return all_caps


# ── Diff logic ───────────────────────────────────────────────────────────────

def compute_diff(
    extracted: list[dict],
    processed: dict,
) -> tuple[list[dict], list[dict], list[dict]]:
    """Compare extracted capabilities against processed ones.

    Returns: (new, changed, removed)
    """
    new = []
    changed = []
    removed_ids = set(processed.keys())

    for cap in extracted:
        cap_id = cap["id"]
        removed_ids.discard(cap_id)

        if cap_id not in processed:
            new.append(cap)
        else:
            prev = processed[cap_id]
            if prev.get("source_hash") != cap["source_hash"]:
                changed.append(cap)
            elif not prev.get("java_idiom"):
                # Previously saved but never enriched (e.g. API key was missing)
                new.append(cap)

    removed = [{"id": rid, **processed[rid]} for rid in removed_ids]

    return new, changed, removed


# ── Haiku enrichment ─────────────────────────────────────────────────────────

HAIKU_PROMPT_TEMPLATE = """\
You are generating binding guidance for a multi-language RPC framework called Aster (P2P RPC over QUIC/iroh).

Given this Python capability:

  Module: {module}
  Class: {class_name}
  Method: {method_name}
  Params: {params}
  Returns: {return_type}
  Async: {is_async}
  Decorators: {decorators}
  Docstring: {docstring}
  Security flags detected: {security_flags}

Source code:
```python
{source_code}
```

Provide a JSON object with:
1. "summary": One-sentence description of what this does based on the actual implementation (max 100 chars)
2. "semantic_category": One of: transport, rpc_framework, codec, interceptor, trust, registry, config, health, metadata
3. "java_idiom": How this should look in idiomatic Java 21+ (method signature + brief note). Use records, sealed interfaces, CompletableFuture, virtual threads where appropriate.
4. "kotlin_idiom": How this should look in idiomatic Kotlin (method signature + brief note). Use coroutines, data classes, sealed classes, Flow where appropriate.
5. "go_idiom": How this should look in idiomatic Go (function signature + brief note). Use error returns, context.Context for cancellation, channels for streaming.
6. "security_notes": Based on the source code: if this capability handles untrusted input, credentials, crypto, or network I/O, what must the implementer get right in a new language binding? Focus on: size limits, timeout enforcement, no oracle leaks, input validation. Empty string if no security concerns.
7. "notes": Any non-obvious mapping considerations (empty string if none)

Return ONLY the JSON object, no markdown fences."""


def enrich_with_haiku(caps: list[dict]) -> list[dict]:
    """Call Claude Haiku via the `claude` CLI to generate idiom hints."""
    import subprocess
    import shutil

    claude_bin = shutil.which("claude")
    if not claude_bin:
        print("  `claude` CLI not found — skipping enrichment")
        print("  Install with: npm install -g @anthropic-ai/claude-code")
        return caps

    enriched = []
    total = len(caps)
    errors = 0

    for i, cap in enumerate(caps, 1):
        print(f"  [{i}/{total}] {cap['id']}...", end=" ", flush=True)

        prompt = HAIKU_PROMPT_TEMPLATE.format(
            module=cap.get("module", ""),
            class_name=cap.get("class_name", "None"),
            method_name=cap.get("method_name", ""),
            params=cap.get("params", []),
            return_type=cap.get("return_type", ""),
            is_async=cap.get("is_async", False),
            decorators=cap.get("decorators", []),
            docstring=cap.get("docstring", ""),
            security_flags=cap.get("security_flags", []),
            source_code=cap.get("source_code", "(source not available)"),
        )

        try:
            result = subprocess.run(
                [claude_bin, "-p", prompt, "--model", "haiku", "--output-format", "text"],
                capture_output=True,
                text=True,
                timeout=30,
            )

            if result.returncode != 0:
                print(f"error (exit {result.returncode})")
                cap["error"] = result.stderr[:200]
                errors += 1
                enriched.append(cap)
                continue

            text = result.stdout.strip()
            # Strip markdown fences if present
            if text.startswith("```"):
                text = "\n".join(text.split("\n")[1:])
            if text.endswith("```"):
                text = "\n".join(text.split("\n")[:-1])
            text = text.strip()

            idioms = json.loads(text)
            cap["summary"] = idioms.get("summary", "")
            cap["semantic_category"] = idioms.get("semantic_category", "")
            cap["java_idiom"] = idioms.get("java_idiom", "")
            cap["kotlin_idiom"] = idioms.get("kotlin_idiom", "")
            cap["go_idiom"] = idioms.get("go_idiom", "")
            cap["security_notes"] = idioms.get("security_notes", "")
            cap["idiom_notes"] = idioms.get("notes", "")
            cap["last_updated"] = datetime.now(timezone.utc).isoformat()
            print("ok")
        except subprocess.TimeoutExpired:
            print("timeout")
            cap["error"] = "timeout"
            errors += 1
        except json.JSONDecodeError as e:
            print(f"bad json: {e}")
            cap["error"] = f"bad json: {text[:100]}"
            errors += 1
        except Exception as e:
            print(f"error: {e}")
            cap["error"] = str(e)
            errors += 1

        enriched.append(cap)

    if errors:
        print(f"\n  {errors} errors — re-run `process` to retry failed entries")

    return enriched


# ── Commands ─────────────────────────────────────────────────────────────────

def cmd_extract() -> list[dict]:
    """Extract capabilities and write to extracted.yaml."""
    print("Extracting capabilities from Python bindings...")
    caps = extract_all()
    caps_dicts = [asdict(c) for c in caps]

    OUTPUT_DIR.mkdir(parents=True, exist_ok=True)
    dump_yaml(caps_dicts, EXTRACTED_FILE)

    # Stats
    native = sum(1 for c in caps if c.module == "aster._aster")
    framework = len(caps) - native
    print(f"  {len(caps)} capabilities extracted ({native} native, {framework} framework)")
    print(f"  Written to {EXTRACTED_FILE.relative_to(REPO_ROOT)}")

    return caps_dicts


def cmd_diff() -> tuple[list[dict], list[dict], list[dict]]:
    """Show what changed since last processing."""
    if not EXTRACTED_FILE.exists():
        print("No extracted.yaml found — run 'extract' first")
        return [], [], []

    extracted = load_yaml(EXTRACTED_FILE)
    processed = {}
    if PROCESSED_FILE.exists():
        proc_list = load_yaml(PROCESSED_FILE)
        if isinstance(proc_list, list):
            processed = {item["id"]: item for item in proc_list}
        elif isinstance(proc_list, dict):
            processed = proc_list

    new, changed, removed = compute_diff(extracted, processed)

    print(f"  New: {len(new)}")
    if new:
        for c in new[:10]:
            print(f"    + {c['id']}")
        if len(new) > 10:
            print(f"    ... and {len(new) - 10} more")

    print(f"  Changed: {len(changed)}")
    if changed:
        for c in changed[:10]:
            print(f"    ~ {c['id']}")
        if len(changed) > 10:
            print(f"    ... and {len(changed) - 10} more")

    print(f"  Removed: {len(removed)}")
    if removed:
        for c in removed[:10]:
            print(f"    - {c['id']}")

    return new, changed, removed


def cmd_process() -> None:
    """Process new/changed capabilities through Haiku."""
    new, changed, removed = cmd_diff()

    to_process = new + changed
    if not to_process:
        print("  Nothing to process — all capabilities up to date")
        return

    print(f"\nProcessing {len(to_process)} capabilities through Haiku...")
    enriched = enrich_with_haiku(to_process)

    # Merge with existing processed
    processed = {}
    if PROCESSED_FILE.exists():
        proc_list = load_yaml(PROCESSED_FILE)
        if isinstance(proc_list, list):
            processed = {item["id"]: item for item in proc_list}

    # Remove deleted
    for r in removed:
        processed.pop(r["id"], None)

    # Update with enriched (strip source_code to keep file lean)
    for cap in enriched:
        clean = {k: v for k, v in cap.items() if k != "source_code"}
        processed[clean["id"]] = clean

    # Write back as list
    dump_yaml(list(processed.values()), PROCESSED_FILE)
    print(f"\n  Written to {PROCESSED_FILE.relative_to(REPO_ROOT)}")


def cmd_run() -> None:
    """Full pipeline: extract → diff → process."""
    cmd_extract()
    print()
    cmd_process()


def cmd_stats() -> None:
    """Show statistics about the capability catalog."""
    if not EXTRACTED_FILE.exists():
        print("No extracted.yaml found — run 'extract' first")
        return

    extracted = load_yaml(EXTRACTED_FILE)

    # Group by module
    by_module: dict[str, int] = {}
    by_class: dict[str, int] = {}
    async_count = 0
    for cap in extracted:
        mod = cap.get("module", "unknown")
        by_module[mod] = by_module.get(mod, 0) + 1
        cls = cap.get("class_name")
        if cls:
            by_class[cls] = by_class.get(cls, 0) + 1
        if cap.get("is_async"):
            async_count += 1

    # Security stats
    sec_flagged = 0
    sec_counts: dict[str, int] = {}
    for cap in extracted:
        flags = cap.get("security_flags", [])
        if flags:
            sec_flagged += 1
        for f in flags:
            sec_counts[f] = sec_counts.get(f, 0) + 1

    print(f"Total capabilities: {len(extracted)}")
    print(f"  Async: {async_count}")
    print(f"  Sync: {len(extracted) - async_count}")
    print(f"  Security-flagged: {sec_flagged}")
    if sec_counts:
        print(f"\nSecurity flags:")
        for flag in sorted(sec_counts, key=sec_counts.get, reverse=True):
            print(f"  {flag}: {sec_counts[flag]}")
    print(f"\nBy module ({len(by_module)}):")
    for mod in sorted(by_module, key=by_module.get, reverse=True)[:15]:
        print(f"  {mod}: {by_module[mod]}")
    if len(by_module) > 15:
        print(f"  ... and {len(by_module) - 15} more")

    print(f"\nBy class ({len(by_class)}):")
    for cls in sorted(by_class, key=by_class.get, reverse=True)[:15]:
        print(f"  {cls}: {by_class[cls]}")
    if len(by_class) > 15:
        print(f"  ... and {len(by_class) - 15} more")

    # Processed stats
    if PROCESSED_FILE.exists():
        processed = load_yaml(PROCESSED_FILE)
        if isinstance(processed, list):
            proc_count = len(processed)
        else:
            proc_count = len(processed)
        enriched = sum(1 for p in (processed if isinstance(processed, list) else processed.values())
                       if isinstance(p, dict) and p.get("java_idiom"))
        print(f"\nProcessed: {proc_count} / {len(extracted)}")
        print(f"  With idiom hints: {enriched}")


# ── Write default mapping file ───────────────────────────────────────────────

DEFAULT_MAPPING = {
    "type_mapping": {
        "python_to_java": {
            "str": "String",
            "int": "long",
            "float": "double",
            "bool": "boolean",
            "bytes": "byte[]",
            "list": "List<T>",
            "dict": "Map<K, V>",
            "tuple": "record or Object[]",
            "None": "void",
            "Optional[T]": "@Nullable T",
            "Any": "Object",
        },
        "python_to_kotlin": {
            "str": "String",
            "int": "Long",
            "float": "Double",
            "bool": "Boolean",
            "bytes": "ByteArray",
            "list": "List<T>",
            "dict": "Map<K, V>",
            "tuple": "data class or Pair/Triple",
            "None": "Unit",
            "Optional[T]": "T?",
            "Any": "Any",
        },
        "python_to_go": {
            "str": "string",
            "int": "int64",
            "float": "float64",
            "bool": "bool",
            "bytes": "[]byte",
            "list": "[]T",
            "dict": "map[K]V",
            "tuple": "struct or multiple returns",
            "None": "error (Go uses error returns)",
            "Optional[T]": "*T or (T, bool)",
            "Any": "interface{}",
        },
    },
    "pattern_mapping": {
        "async_function": {
            "python": "async def / await",
            "java": "CompletableFuture<T>",
            "kotlin": "suspend fun",
            "go": "func() (T, error) — goroutines for concurrency",
        },
        "decorator": {
            "python": "@decorator",
            "java": "@Annotation",
            "kotlin": "@Annotation",
            "go": "No direct equivalent — use struct tags or code generation",
        },
        "dataclass": {
            "python": "@dataclass",
            "java": "record (Java 16+) or POJO with Lombok",
            "kotlin": "data class",
            "go": "struct",
        },
        "context_manager": {
            "python": "async with / __aenter__ / __aexit__",
            "java": "try-with-resources (AutoCloseable)",
            "kotlin": "use {} extension",
            "go": "defer cleanup()",
        },
        "async_iterator": {
            "python": "async for / __aiter__ / __anext__",
            "java": "Flow<T> (reactive) or Iterator<CompletableFuture<T>>",
            "kotlin": "Flow<T>",
            "go": "chan T or callback func(T)",
        },
        "protocol_class": {
            "python": "Protocol (structural typing)",
            "java": "interface",
            "kotlin": "interface",
            "go": "interface (implicit satisfaction)",
        },
        "enum": {
            "python": "enum.Enum / IntEnum",
            "java": "enum",
            "kotlin": "enum class",
            "go": "const iota block",
        },
        "class_method_factory": {
            "python": "@classmethod or async factory",
            "java": "static factory method",
            "kotlin": "companion object fun",
            "go": "NewXxx() constructor function",
        },
    },
}


def ensure_mapping_file() -> None:
    """Write the default mapping file if it doesn't exist."""
    OUTPUT_DIR.mkdir(parents=True, exist_ok=True)
    if not MAPPING_FILE.exists():
        dump_yaml(DEFAULT_MAPPING, MAPPING_FILE)
        print(f"  Created {MAPPING_FILE.relative_to(REPO_ROOT)}")


# ── Main ─────────────────────────────────────────────────────────────────────

def main() -> None:
    ensure_mapping_file()

    if len(sys.argv) < 2:
        print("Usage: capability_scan.py <command>")
        print("Commands: extract, diff, process, run, stats")
        sys.exit(1)

    cmd = sys.argv[1]
    if cmd == "extract":
        cmd_extract()
    elif cmd == "diff":
        cmd_diff()
    elif cmd == "process":
        cmd_process()
    elif cmd == "run":
        cmd_run()
    elif cmd == "stats":
        cmd_stats()
    else:
        print(f"Unknown command: {cmd}")
        sys.exit(1)


if __name__ == "__main__":
    main()
