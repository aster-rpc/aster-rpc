"""
aster_cli.init -- ``aster init`` command.

Generates project scaffolding files:
- ``aster init --ai`` writes an ``AGENTS_aster.md`` guide for AI coding
  assistants, and appends a link to ``AGENTS.md``.
"""

from __future__ import annotations

import importlib.resources
import sys
from pathlib import Path


_AGENTS_LINK = "See [Aster RPC Guide](AGENTS_aster.md) for building Aster services."
_AGENTS_FILE = "AGENTS_aster.md"


def _aster_version() -> str:
    """Return the installed aster version, or 'unknown'."""
    try:
        from importlib.metadata import version
        return version("aster-python")
    except Exception:
        return "unknown"


def _load_template(language: str) -> str:
    """Load a bundled LLM template for the given language."""
    template_dir = Path(__file__).parent / "templates" / "llm"
    template_file = template_dir / f"{language}.md"
    if not template_file.exists():
        available = [f.stem for f in template_dir.glob("*.md")]
        print(
            f"Error: no template for language '{language}'. "
            f"Available: {', '.join(available) or 'none'}",
            file=sys.stderr,
        )
        sys.exit(1)
    text = template_file.read_text()
    text = text.replace("{{aster_version}}", _aster_version())
    return text


def _ensure_agents_link(project_dir: Path) -> None:
    """Append a link to AGENTS_aster.md in AGENTS.md (idempotent)."""
    agents_md = project_dir / "AGENTS.md"

    if agents_md.exists():
        text = agents_md.read_text()
        if _AGENTS_FILE in text:
            return  # link already present
        if not text.endswith("\n"):
            text += "\n"
        text += f"\n{_AGENTS_LINK}\n"
        agents_md.write_text(text)
        print(f"Appended Aster link to {agents_md}")
    else:
        agents_md.write_text(f"{_AGENTS_LINK}\n")
        print(f"Created {agents_md}")


def cmd_init_ai(args) -> int:
    """Execute ``aster init --ai``."""
    language = args.language or "python"
    project_dir = Path(args.dir or ".")

    content = _load_template(language)

    out_path = project_dir / _AGENTS_FILE
    out_path.write_text(content)
    print(f"Wrote {out_path} ({language})")

    _ensure_agents_link(project_dir)
    return 0


# ── Argparse registration ────────────────────────────────────────────────


def register_init_subparser(subparsers) -> None:
    init_parser = subparsers.add_parser(
        "init", help="Initialize project scaffolding",
    )
    init_parser.add_argument(
        "--ai", action="store_true", default=False,
        help="Generate AGENTS_aster.md for AI coding assistants",
    )
    init_parser.add_argument(
        "--language", "-l", default=None,
        help="Language for the AI guide (default: python)",
    )
    init_parser.add_argument(
        "--dir", "-d", default=None,
        help="Project directory (default: current directory)",
    )


def run_init_command(args) -> int:
    if args.ai:
        return cmd_init_ai(args)
    print("Usage: aster init --ai [--language python|typescript]", file=sys.stderr)
    return 1
