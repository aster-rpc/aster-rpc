#!/usr/bin/env python3
"""
combine_spec.py — Combine spec/spec_sections/*.md back into spec/SPEC.md.

Strips YAML frontmatter from each section file before concatenating.
Section order is determined by filename sort (00-, 01-, 02- … A-, B-, C-).

Usage:
    python scripts/combine_spec.py
    python scripts/combine_spec.py --sections-dir spec/spec_sections --out spec/SPEC.md
    python scripts/combine_spec.py --dry-run        # print stats, don't write
    python scripts/combine_spec.py --exclude 00-session-state.md 01-document-map.md
"""

import re
import sys
import argparse
from pathlib import Path

# ---------------------------------------------------------------------------
# Config
# ---------------------------------------------------------------------------

PROJECT_ROOT  = Path(__file__).parent.parent
DEFAULT_IN    = PROJECT_ROOT / "spec" / "spec_sections"
DEFAULT_OUT   = PROJECT_ROOT / "spec" / "SPEC.md"

# Files excluded from the combined output by default.
# These are authoring artefacts, not normative spec content.
DEFAULT_EXCLUDE = ["00-session-state.md", "01-document-map.md"]

SEPARATOR = "\n\n"   # between sections

# ---------------------------------------------------------------------------
# Frontmatter stripping
# ---------------------------------------------------------------------------

def strip_frontmatter(text: str) -> str:
    """Remove a YAML frontmatter block (--- ... ---) from the top of the text."""
    if not text.startswith("---\n"):
        return text
    end = text.find("\n---\n", 4)
    if end == -1:
        return text   # malformed — leave as-is
    return text[end + 5:]   # skip '\n---\n'

# ---------------------------------------------------------------------------
# File ordering
# ---------------------------------------------------------------------------

def sort_key(path: Path) -> tuple:
    """
    Sort spec section files in the correct order:
    numeric prefix (00, 01, 02 …) < letter prefix (A, B, C)
    """
    name = path.stem   # e.g. "03-action-layer"
    m = re.match(r"^(\d+)", name)
    if m:
        return (0, int(m.group(1)), name)
    # Letter prefix (A-, B-, C-)
    m = re.match(r"^([A-Z])", name)
    if m:
        return (1, ord(m.group(1)), name)
    return (2, 0, name)

def collect_files(sections_dir: Path, exclude: list[str]) -> list[Path]:
    files = [f for f in sections_dir.glob("*.md") if f.name not in exclude]
    return sorted(files, key=sort_key)

# ---------------------------------------------------------------------------
# Main
# ---------------------------------------------------------------------------

def main():
    parser = argparse.ArgumentParser(description="Combine spec sections into SPEC.md.")
    parser.add_argument("--sections-dir", type=Path, default=DEFAULT_IN)
    parser.add_argument("--out",          type=Path, default=DEFAULT_OUT)
    parser.add_argument("--dry-run",      action="store_true")
    parser.add_argument(
        "--exclude",
        nargs="*",
        default=DEFAULT_EXCLUDE,
        metavar="FILENAME",
        help="Section filenames to exclude (default: session-state + document-map authoring files).",
    )
    parser.add_argument(
        "--include-all",
        action="store_true",
        help="Include ALL section files including authoring artefacts (overrides --exclude).",
    )
    args = parser.parse_args()

    sections_dir: Path = args.sections_dir
    out_path:     Path = args.out
    exclude = [] if args.include_all else args.exclude

    if not sections_dir.exists():
        print(f"ERROR: sections directory not found: {sections_dir}", file=sys.stderr)
        print("       Run split_spec.py first.", file=sys.stderr)
        sys.exit(1)

    files = collect_files(sections_dir, exclude)

    if not files:
        print(f"ERROR: no .md files found in {sections_dir}", file=sys.stderr)
        sys.exit(1)

    print(f"Combining {len(files)} sections → {out_path}\n")
    if exclude:
        print(f"  Excluding: {', '.join(exclude)}")

    parts = []
    total_lines = 0

    for f in files:
        raw = f.read_text(encoding="utf-8")
        body = strip_frontmatter(raw).rstrip("\n")
        line_count = body.count("\n") + 1
        total_lines += line_count
        parts.append(body)
        print(f"  + {f.name:<40}  {line_count:>5} lines")

    combined = SEPARATOR.join(parts) + "\n"

    print(f"\n  Total: {total_lines} lines across {len(parts)} sections")

    if args.dry_run:
        print("\n[dry-run] No files written.")
        return

    # Write output
    out_path.parent.mkdir(parents=True, exist_ok=True)
    out_path.write_text(combined, encoding="utf-8")

    out_kb = out_path.stat().st_size // 1024
    print(f"\n✓ Written: {out_path}  ({out_kb} KB)")


if __name__ == "__main__":
    main()