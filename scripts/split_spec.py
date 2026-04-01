#!/usr/bin/env python3
"""
split_spec.py — Split SPEC.md into per-section files.

Writes to: spec/spec_sections/
- Adds YAML frontmatter to each file (title, section id, status, editors)
- Adds {#anchor} to every ## and ### heading (idempotent — won't double-add)
- Preserves all content exactly

Usage:
    python scripts/split_spec.py
    python scripts/split_spec.py --spec-path spec/SPEC.md --out-dir spec/spec_sections
"""

import re
import sys
import argparse
from pathlib import Path
from datetime import date

# ---------------------------------------------------------------------------
# Config
# ---------------------------------------------------------------------------

PROJECT_ROOT = Path(__file__).parent.parent
DEFAULT_SPEC  = PROJECT_ROOT / "spec" / "Aster-SPEC.md"
DEFAULT_OUT   = PROJECT_ROOT / "spec" / "Aster-spec_sections"

EDITORS = ["Emrul Islam"]

# Map each top-level ## heading (matched by regex against the heading text)
# to a stable filename prefix and section id.
# Order matters — first match wins.
SECTION_MAP = [
    # (pattern,                            filename_prefix,  section_id)
    (r"^Session State",                    "00-session-state",      "session-state"),
    (r"^Document Map",                     "01-document-map",       "document-map"),
    (r"^1\.\s+Foundations",               "02-foundations",         "foundations"),
    (r"^3\.\s+Action Layer",              "03-action-layer",        "action-layer"),
    (r"^4\.\s+Process Layer",             "04-process-layer",       "process-layer"),
    (r"^5\.\s+Supporting Resources",      "05-supporting-resources","supporting-resources"),
    (r"^6\.\s+Execution Model",           "06-execution-model",     "execution-model"),
    (r"^7\.\s+Validation",                "07-validation",          "validation"),
    (r"^8\.\s+Visuali",                   "08-visualisation",       "visualisation"),
    (r"^Appendix A",                       "A-bpm-mapping",          "appendix-a"),
    (r"^Appendix B",                       "B-glossary",             "appendix-b"),
    (r"^Appendix C",                       "C-schema-reference",     "appendix-c"),
]

# ---------------------------------------------------------------------------
# Anchor generation
# ---------------------------------------------------------------------------

_ANCHOR_RE = re.compile(r"\s*\{#[^}]+\}\s*$")

def _to_slug(text: str) -> str:
    """Convert heading text to a URL-friendly anchor slug."""
    # Strip leading section numbers like "3.1 ", "A — ", "4.11.2 "
    text = re.sub(r"^[\dA-Z]+[\.\s—–-]+\s*", "", text)
    text = text.lower()
    text = re.sub(r"[^\w\s-]", "", text)      # strip punctuation
    text = re.sub(r"[\s_]+", "-", text.strip())
    text = re.sub(r"-{2,}", "-", text)
    return text or "section"

def _section_anchor(heading_text: str) -> str:
    """
    Generate anchor for a ## heading.
    e.g. "3. Action Layer" → "action-layer"
         "Appendix A — BPM 2.0 Concept Mapping" → "appendix-a"
    """
    # Special-case appendices so they stay short and stable
    m = re.match(r"^Appendix\s+([A-Z])", heading_text)
    if m:
        return f"appendix-{m.group(1).lower()}"
    return _to_slug(heading_text)

def _subsection_anchor(heading_text: str) -> str:
    """
    Generate anchor for a ### heading, preserving the section number prefix.
    e.g. "3.1 Purpose and Scope" → "s3-1-purpose-and-scope"
         "1.2 Spec Axioms" → "s1-2-spec-axioms"
    """
    m = re.match(r"^([\d]+(?:\.[\d]+)*)\s+(.*)", heading_text)
    if m:
        num_slug = m.group(1).replace(".", "-")
        title_slug = _to_slug(m.group(2))
        return f"s{num_slug}-{title_slug}"
    return _to_slug(heading_text)

def add_anchor(line: str) -> str:
    """Add {#anchor} to a ## or ### heading line if not already present."""
    # Already has an anchor — leave it alone
    if _ANCHOR_RE.search(line):
        return line

    m2 = re.match(r"^(#{2})\s+(.+)", line)
    if m2:
        hashes, title = m2.group(1), m2.group(2).strip()
        slug = _section_anchor(title)
        return f"{hashes} {title} {{#{slug}}}\n"

    m3 = re.match(r"^(#{3})\s+(.+)", line)
    if m3:
        hashes, title = m3.group(1), m3.group(2).strip()
        slug = _subsection_anchor(title)
        return f"{hashes} {title} {{#{slug}}}\n"

    return line

# ---------------------------------------------------------------------------
# Frontmatter
# ---------------------------------------------------------------------------

def make_frontmatter(title: str, section_id: str, status: str = "draft") -> str:
    editors_yaml = "\n".join(f'  - name: "{e}"' for e in EDITORS)
    return f"""---
title: "{title}"
section: "{section_id}"
status: {status}
editors:
{editors_yaml}
generated: false
---
"""

def strip_frontmatter(text: str) -> str:
    """Remove YAML frontmatter block from the top of a file if present."""
    if text.startswith("---\n"):
        end = text.find("\n---\n", 4)
        if end != -1:
            return text[end + 5:]  # skip closing --- and newline
    return text

# ---------------------------------------------------------------------------
# Parsing
# ---------------------------------------------------------------------------

def classify_h2(heading_text: str):
    """Return (filename_prefix, section_id) for a ## heading, or None if unknown."""
    for pattern, prefix, sid in SECTION_MAP:
        if re.search(pattern, heading_text):
            return prefix, sid
    return None

def split_spec(spec_path: Path) -> list[dict]:
    """
    Parse SPEC.md into chunks, one per ## section plus a preamble chunk.
    Returns list of dicts: {filename, section_id, title, lines}
    """
    raw = spec_path.read_text(encoding="utf-8")
    all_lines = raw.splitlines(keepends=True)

    chunks = []
    current = None

    for line in all_lines:
        m = re.match(r"^## (.+)", line)
        if m:
            heading_text = m.group(1).strip()
            info = classify_h2(heading_text)
            if info is None:
                print(f"  WARNING: unrecognised ## heading: {heading_text!r}", file=sys.stderr)
                if current is not None:
                    current["lines"].append(line)
                continue
            # Save previous chunk
            if current is not None:
                chunks.append(current)
            filename_prefix, section_id = info
            current = {
                "filename": f"{filename_prefix}.md",
                "section_id": section_id,
                "title": heading_text,
                "lines": [line],
            }
        else:
            if current is None:
                # Lines before the first ## heading → preamble
                if not chunks and not any(c.get("is_preamble") for c in chunks):
                    current = {
                        "filename": "00-preamble.md",
                        "section_id": "preamble",
                        "title": "Preamble",
                        "lines": [],
                        "is_preamble": True,
                    }
                    # re-add this line to current
                current["lines"].append(line) if current else None
            else:
                current["lines"].append(line)

    if current is not None:
        chunks.append(current)

    return chunks

# ---------------------------------------------------------------------------
# Main
# ---------------------------------------------------------------------------

def main():
    parser = argparse.ArgumentParser(description="Split SPEC.md into per-section files.")
    parser.add_argument("--spec-path", type=Path, default=DEFAULT_SPEC)
    parser.add_argument("--out-dir",   type=Path, default=DEFAULT_OUT)
    parser.add_argument("--dry-run",   action="store_true", help="Print what would be written, don't write")
    args = parser.parse_args()

    spec_path: Path = args.spec_path
    out_dir:   Path = args.out_dir

    if not spec_path.exists():
        print(f"ERROR: spec file not found: {spec_path}", file=sys.stderr)
        sys.exit(1)

    print(f"Reading: {spec_path}  ({spec_path.stat().st_size // 1024} KB)")
    chunks = split_spec(spec_path)

    if not args.dry_run:
        out_dir.mkdir(parents=True, exist_ok=True)

    print(f"\nSplitting into {len(chunks)} files → {out_dir}/\n")

    for chunk in chunks:
        filename   = chunk["filename"]
        section_id = chunk["section_id"]
        title      = chunk["title"]
        lines      = chunk["lines"]

        # Apply anchors to ## and ### headings
        processed = [add_anchor(l) for l in lines]

        # Determine status from content (look for DRAFT/STABLE/TODO markers)
        body = "".join(processed)
        if "Status: STABLE" in body or "> **Status: STABLE**" in body:
            status = "stable"
        elif "Status: TODO" in body or "> **Status: TODO**" in body:
            status = "todo"
        else:
            status = "draft"

        frontmatter = make_frontmatter(title, section_id, status)
        full_content = frontmatter + "".join(processed)

        out_path = out_dir / filename
        line_count = len(processed)

        print(f"  → {filename:<40}  {line_count:>5} lines  [{status}]")

        if not args.dry_run:
            out_path.write_text(full_content, encoding="utf-8")

    if not args.dry_run:
        print(f"\n✓ Written {len(chunks)} files to {out_dir}")
        print("\nNOTE: Review 00-session-state.md and 01-document-map.md —")
        print("      these are authoring artefacts and should be excluded from")
        print("      the published spec (mkdocs.yml nav) but kept in the repo.")
    else:
        print("\n[dry-run] No files written.")


if __name__ == "__main__":
    main()