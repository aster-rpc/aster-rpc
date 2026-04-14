#!/usr/bin/env python3
"""
capability_compare.py -- Compare Python capability surface against TypeScript.

Reads the extracted Python capabilities, searches the TypeScript codebase for
matching classes/methods, and reports coverage gaps.

Usage:
    python scripts/capability_compare.py
"""

from __future__ import annotations

import json
import re
import subprocess
from pathlib import Path
from collections import defaultdict

REPO_ROOT = Path(__file__).resolve().parent.parent
EXTRACTED = REPO_ROOT / "docs" / "_internal" / "capabilities" / "extracted.yaml"
PROCESSED = REPO_ROOT / "docs" / "_internal" / "capabilities" / "processed.yaml"
TS_SRC = REPO_ROOT / "bindings" / "typescript" / "packages" / "aster" / "src"


def to_camel(name: str) -> str:
    """snake_case to camelCase."""
    parts = name.split("_")
    return parts[0] + "".join(p.capitalize() for p in parts[1:])


def to_pascal(name: str) -> str:
    """snake_case to PascalCase."""
    return "".join(p.capitalize() for p in name.split("_"))


# Capabilities handled by Rust core -- all bindings get these for free via FFI.
# Exclude from cross-binding comparison.
RUST_CORE_DUPLICATES = {
    # canonical.py -- 100% duplicate of core/src/canonical.rs
    "write_varint", "write_zigzag_i32", "write_zigzag_i64", "write_string",
    "write_bytes_field", "write_bool", "write_float64", "write_list_header",
    "write_optional_absent", "write_optional_present_prefix",
    "CanonicalWriter.varint", "CanonicalWriter.zigzag_i32",
    "CanonicalWriter.zigzag_i64", "CanonicalWriter.string",
    "CanonicalWriter.bytes_field", "CanonicalWriter.bool_",
    "CanonicalWriter.float64", "CanonicalWriter.list_header",
    "CanonicalWriter.optional_absent", "CanonicalWriter.optional_present_prefix",
    "CanonicalWriter.raw", "CanonicalWriter.getvalue",
    # signing.py -- signing bytes construction duplicated in core/src/signing.rs
    "canonical_json", "canonical_signing_bytes",
    # identity.py -- hash computation delegated to Rust core
    "compute_type_hash", "normalize_identifier",
    "resolve_with_cycles",  # orchestration around Rust tarjan_scc
}


# Known Python → TypeScript class name mappings
CLASS_MAP = {
    "Server": ["RpcServer", "Server"],
    "AsterServer": ["AsterServer"],
    "AsterClient": ["AsterClientWrapper", "AsterClient"],
    "ServiceClient": ["ServiceClient"],
    "ForyCodec": ["ForyCodec", "JsonCodec"],
    "IrohTransport": ["IrohTransport"],
    "LocalTransport": ["LocalTransport"],
    "ServiceRegistry": ["ServiceRegistry"],
    "CanonicalWriter": ["CanonicalWriter"],
    "InterceptedBidiChannel": ["InterceptedBidiChannel"],
    "Metadata": ["Metadata"],
    "ConnectionMetrics": ["ConnectionMetrics"],
    "AdmissionMetrics": ["AdmissionMetrics"],
    "HealthServer": ["HealthServer"],
    "ClockDriftDetector": ["ClockDriftTracker", "ClockDriftDetector"],
    "DynamicTypeFactory": ["DynamicTypeFactory"],
    "MeshEndpointHook": ["MeshEndpointHook"],
    "RegistryGossip": ["RegistryGossip"],
    "RegistryPublisher": ["RegistryPublisher"],
    "RegistryACL": ["RegistryACL"],
    "NonceStore": ["InMemoryNonceStore", "NonceStore"],
    "InMemoryNonceStore": ["InMemoryNonceStore"],
    "MeshState": ["MeshState"],
    "AsterConfig": ["AsterConfig"],
    "ContractManifest": ["ContractManifest", "FatalContractMismatch"],
    "ForyConfig": ["ForyConfig", "ResolvedForyConfig"],
    "EndpointLease": ["EndpointLease"],
    "ServiceSummary": ["ServiceSummary"],
    "RegistryClient": ["RegistryClient"],
}

# Known Python → TypeScript method name mappings (overrides)
METHOD_MAP = {
    "start": ["start", "serve"],
    "__init__": ["constructor"],
    "__aenter__": [],  # Python-only
    "__aexit__": [],   # Python-only
    "__aiter__": [],   # Python-only
    "__anext__": [],   # Python-only
    # Python health methods → TS equivalents
    "connection_opened": ["onAccept", "connectionOpened"],
    "connection_closed": ["onClose", "connectionClosed"],
    "stream_opened": ["onStreamOpen", "streamOpened"],
    "stream_closed": ["onStreamClose", "streamClosed"],
    "record_consumer_admit": ["onSuccess", "recordConsumerAdmit"],
    "record_consumer_deny": ["onReject", "recordConsumerDeny"],
    "record_consumer_error": ["onError", "recordConsumerError"],
    "record_producer_admit": ["onSuccess", "recordProducerAdmit"],
    "record_producer_deny": ["onReject", "recordProducerDeny"],
    "record_producer_error": ["onError", "recordProducerError"],
    "to_dict": ["snapshot", "toDict", "toJSON"],
    # Config
    "from_env": ["configFromEnv", "fromEnv"],
    "from_file": ["configFromFile", "fromFile"],
    "resolve_root_pubkey": ["resolveRootPubkey"],
    "to_endpoint_config": ["toEndpointConfig"],
    # Drift
    "track_offset": ["trackOffset", "observe"],
    "mesh_median_offset": ["meshMedianOffset", "medianOffset"],
    "peer_in_drift": ["peerInDrift", "shouldIsolate"],
    "self_in_drift": ["selfInDrift", "shouldIsolate"],
    "peer_offsets": ["peerOffsets"],
    "remove_peer": ["removePeer"],
    # Registry models
    "is_fresh": ["isFresh", "isLeaseFresh"],
    "is_routable": ["isRoutable", "isLeaseRoutable"],
    "to_json_dict": ["toJson", "toJSON", "toJsonDict"],
    "from_json_dict": ["fromJson", "fromJSON", "fromJsonDict"],
    # Nonce
    "is_consumed": ["has", "isConsumed"],
    # ACL
    "remove_writer": ["removeWriter"],
    "get_writers": ["getWriters"],
    "get_readers": ["getReaders"],
    "get_admins": ["getAdmins"],
    # Service
    "get_method": ["getMethod"],
    "has_method": ["hasMethod"],
    "get_all_services": ["getAllServices", "getAll"],
    "get_default_registry": ["getDefaultRegistry"],
    "set_default_registry": ["setDefaultRegistry"],
    # Client
    "service_name": ["serviceName"],
    "service_version": ["serviceVersion"],
    # High-level
    "endpoint_addr_b64": ["endpointAddrB64"],
    "rpc_addr_b64": ["rpcAddrB64"],
    "mesh_state": ["meshState"],
    "root_pubkey": ["rootPubkey"],
    # IID -- Python snake_case vs TS SCREAMING_CASE for acronyms
    "get_iid_backend": ["getIIDBackend"],
    "verify_iid": ["verifyIID"],
    # Signing -- Python name vs TS name
    "generate_root_keypair": ["generateRootKeypair", "generateKeypair"],
    "load_private_key": ["loadPrivateKey"],
    "load_public_key": ["loadPublicKey"],
    "verify_signature": ["verifySignature", "verify"],
    "sign_credential": ["signCredential"],
    # Session: the old Phase-8 create_session/create_local_session surface
    # was retired with the multiplexed-streams migration. Session lifecycle
    # is now driven by AsterClient.open_session() + ClientSession.client().
    "open_session": ["openSession"],
    # Config
    "load_endpoint_config": ["loadEndpointConfig", "configFromFile"],
    "resolve_root_pubkey": ["resolveRootPubkey"],
    "to_endpoint_config": ["toEndpointConfig"],
    # Server / high-level
    "wait_until_stopped": ["waitUntilStopped"],
    "registry_ticket": ["registryTicket"],
    # Transport local
    "remote_id": ["remoteId"],
    # Codec
    "wire_type": ["wireType", "WIRE_TYPE_KEY", "getWireType"],
    "resolved_xlang": ["resolvedXlang"],
    "to_kwargs": ["toKwargs"],
    "encode_row_schema": ["encodeRowSchema"],
    "decode_row_data": ["decodeRowData"],
    "registered_types": ["registeredTypes"],
    # Contract
    "save": ["save", "saveManifest"],
    "contract_id_from_service": ["contractIdFromService"],
    "build_type_graph": ["buildTypeGraph", "walkTypeGraph"],
    "from_service_info": ["fromServiceInfo"],
    "extract_method_descriptors": ["extractMethodDescriptors"],
    # Registry
    "broadcast_compatibility_published": ["broadcastCompatibilityPublished"],
    "resolve_all": ["resolveAll"],
    "fetch_contract": ["fetchContract"],
    "on_change": ["onChange"],
    # Dynamic
    "register_from_manifest": ["registerFromManifest"],
    "get_type": ["getType"],
    "get_all_types": ["getAllTypes"],
    "build_request": ["buildRequest"],
    "type_count": ["typeCount"],
    # Client extras
    "create_local_client": ["createLocalClient"],
    "time_sleep": ["timeSleep"],
    "timeouts": ["timeouts"],
}


def load_ts_symbols() -> dict[str, set[str]]:
    """Load all class/method symbols from TypeScript source files.

    Returns: {class_name: {method_name, ...}}
    """
    symbols: dict[str, set[str]] = defaultdict(set)

    if not TS_SRC.exists():
        print(f"TypeScript source not found at {TS_SRC}")
        return symbols

    # Find all .ts files
    ts_files = list(TS_SRC.rglob("*.ts"))

    for ts_file in ts_files:
        if ts_file.name.endswith(".test.ts") or "node_modules" in str(ts_file):
            continue

        try:
            content = ts_file.read_text()
        except Exception:
            continue

        # Extract class declarations
        # Matches: export class Foo, class Foo extends Bar, etc.
        class_matches = re.finditer(
            r'(?:export\s+)?class\s+(\w+)', content
        )
        current_classes = [m.group(1) for m in class_matches]

        # Extract method declarations within classes
        # Matches: async methodName(, methodName(, static methodName(
        method_matches = re.finditer(
            r'(?:async\s+)?(?:static\s+)?(\w+)\s*[\(<]', content
        )

        # Also find standalone export function declarations
        func_matches = re.finditer(
            r'(?:export\s+)?(?:async\s+)?function\s+(\w+)', content
        )

        # For simplicity, associate all methods with all classes in the file
        # (not perfectly accurate but good enough for coverage checking)
        methods_in_file = set()
        for m in method_matches:
            name = m.group(1)
            if name not in ('if', 'for', 'while', 'switch', 'catch', 'return',
                           'new', 'throw', 'import', 'export', 'class', 'interface',
                           'type', 'const', 'let', 'var', 'function', 'extends'):
                methods_in_file.add(name)

        for m in func_matches:
            methods_in_file.add(m.group(1))
            symbols["__module__"].add(m.group(1))

        for cls in current_classes:
            symbols[cls].update(methods_in_file)

    return symbols


def check_capability(
    cap: dict,
    ts_symbols: dict[str, set[str]],
) -> tuple[str, str]:
    """Check if a Python capability exists in TypeScript.

    Returns: (status, detail)
        status: "found", "likely", "missing", "python_only"
    """
    cls_name = cap.get("class_name")
    method = cap.get("method_name", "")

    # Skip Python dunder methods
    if method.startswith("__") and method.endswith("__"):
        return "python_only", f"dunder method {method}"

    # Skip Rust core duplicates -- all bindings get these via FFI
    cap_id = cap.get("id", "")
    if cap_id in RUST_CORE_DUPLICATES:
        return "python_only", f"Rust core duplicate ({cap_id})"

    # Python-specific modules that don't need TS equivalents
    module = cap.get("module", "")
    if module == "aster.logging":
        # Python uses stdlib logging; TS/JS has its own logging patterns
        return "python_only", "Python logging stdlib (TS uses console/pino/etc.)"
    if module == "aster.testing.harness":
        # Test harness is language-specific
        return "python_only", "Python test harness (TS has own test utilities)"

    # Map method name to camelCase
    ts_method = to_camel(method)

    # Also check override mappings
    method_variants = [ts_method, method]
    if method in METHOD_MAP:
        method_variants.extend(METHOD_MAP[method])

    if cls_name is None:
        # Module-level function
        for variant in method_variants:
            if variant in ts_symbols.get("__module__", set()):
                return "found", f"function {variant}"
            # Check all classes too (might be a static method in TS)
            for cls_methods in ts_symbols.values():
                if variant in cls_methods:
                    return "likely", f"found as {variant} (may be in different context)"
        return "missing", f"function {ts_method}"

    # Class method
    ts_class_names = CLASS_MAP.get(cls_name, [cls_name, to_pascal(cls_name)])

    for ts_cls in ts_class_names:
        if ts_cls in ts_symbols:
            for variant in method_variants:
                if variant in ts_symbols[ts_cls]:
                    return "found", f"{ts_cls}.{variant}"

    # Fuzzy: check if method exists in ANY class
    for ts_cls, methods in ts_symbols.items():
        for variant in method_variants:
            if variant in methods:
                return "likely", f"found {variant} in {ts_cls} (expected {cls_name})"

    return "missing", f"{cls_name}.{ts_method}"


def main():
    # Load Python capabilities
    caps = json.loads(EXTRACTED.read_text())

    # Load processed data for categories
    processed = {}
    if PROCESSED.exists():
        proc_list = json.loads(PROCESSED.read_text())
        processed = {p["id"]: p for p in proc_list}

    # Load TypeScript symbols
    print("Scanning TypeScript source...")
    ts_symbols = load_ts_symbols()
    ts_class_count = len([k for k in ts_symbols if k != "__module__"])
    ts_func_count = len(ts_symbols.get("__module__", set()))
    print(f"  Found {ts_class_count} classes, {ts_func_count} module functions\n")

    # Check each capability
    results = {"found": [], "likely": [], "missing": [], "python_only": []}
    by_module = defaultdict(lambda: {"found": 0, "likely": 0, "missing": 0, "python_only": 0, "total": 0})
    by_category = defaultdict(lambda: {"found": 0, "likely": 0, "missing": 0, "total": 0})

    for cap in caps:
        status, detail = check_capability(cap, ts_symbols)
        results[status].append({"id": cap["id"], "module": cap["module"], "detail": detail})

        mod = cap["module"]
        by_module[mod][status] += 1
        by_module[mod]["total"] += 1

        cat = processed.get(cap["id"], {}).get("semantic_category", "unknown")
        if status != "python_only":
            by_category[cat][status] += 1
            by_category[cat]["total"] += 1

    # Summary
    total = len(caps)
    found = len(results["found"])
    likely = len(results["likely"])
    missing = len(results["missing"])
    py_only = len(results["python_only"])
    checkable = total - py_only

    print("=" * 70)
    print(f"CAPABILITY COVERAGE: Python → TypeScript")
    print("=" * 70)
    print(f"  Total Python capabilities: {total}")
    print(f"  Python-only (dunders):     {py_only}")
    print(f"  Checkable:                 {checkable}")
    print(f"  Found in TS:               {found} ({found*100//checkable}%)")
    print(f"  Likely in TS:              {likely} ({likely*100//checkable}%)")
    print(f"  Missing from TS:           {missing} ({missing*100//checkable}%)")
    print()

    # By category
    print("BY CATEGORY:")
    print(f"  {'Category':<20} {'Found':>6} {'Likely':>7} {'Missing':>8} {'Total':>6} {'Coverage':>9}")
    print(f"  {'-'*18:<20} {'-'*5:>6} {'-'*5:>7} {'-'*5:>8} {'-'*5:>6} {'-'*7:>9}")
    for cat in sorted(by_category, key=lambda c: by_category[c]["total"], reverse=True):
        d = by_category[cat]
        cov = (d["found"] + d["likely"]) * 100 // max(d["total"], 1)
        print(f"  {cat:<20} {d['found']:>6} {d['likely']:>7} {d['missing']:>8} {d['total']:>6} {cov:>8}%")
    print()

    # By module (top gaps)
    print("TOP MODULES WITH GAPS:")
    gap_modules = [(m, d) for m, d in by_module.items() if d["missing"] > 0]
    gap_modules.sort(key=lambda x: x[1]["missing"], reverse=True)
    for mod, d in gap_modules[:15]:
        cov = (d["found"] + d["likely"]) * 100 // max(d["total"] - d["python_only"], 1)
        print(f"  {mod:<40} {d['missing']:>3} missing / {d['total'] - d['python_only']:>3} checkable ({cov}% covered)")
    print()

    # List missing capabilities
    print("MISSING CAPABILITIES:")
    for r in sorted(results["missing"], key=lambda x: x["module"]):
        sec = ""
        proc = processed.get(r["id"], {})
        if proc.get("security_flags"):
            sec = " ⚠ SECURITY"
        print(f"  {r['module']:<40} {r['id']:<40}{sec}")

    # Write report
    report_path = REPO_ROOT / "docs" / "_internal" / "capabilities" / "ts_coverage.json"
    report = {
        "summary": {
            "total": total, "checkable": checkable,
            "found": found, "likely": likely, "missing": missing,
            "coverage_pct": (found + likely) * 100 // max(checkable, 1),
        },
        "by_category": dict(by_category),
        "missing": results["missing"],
        "likely": results["likely"],
    }
    report_path.write_text(json.dumps(report, indent=2))
    print(f"\nFull report: {report_path.relative_to(REPO_ROOT)}")


if __name__ == "__main__":
    main()
