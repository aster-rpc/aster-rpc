"""
aster_cli.shell.guide -- First-time guided tour system.

Provides step-by-step hints for new users, triggered by their actions.
The tour state is persisted in ~/.aster/config.toml under [shell] so
it only runs once.

The guide is a sequence of steps, each triggered by a specific event
(command executed, directory entered, etc.). When a step's trigger fires,
we show the hint and advance to the next step.

Custom tours can be registered for domain-specific workflows.
"""

from __future__ import annotations

import os
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any, Callable


@dataclass
class TourStep:
    """A single step in a guided tour."""

    id: str
    trigger: str  # event name that triggers this step
    message: str  # rich-formatted hint to display
    trigger_value: str | None = None  # optional: match on specific value


@dataclass
class Tour:
    """A guided tour -- a sequence of steps shown to first-time users."""

    name: str
    steps: list[TourStep] = field(default_factory=list)
    current_step: int = 0

    @property
    def is_complete(self) -> bool:
        return self.current_step >= len(self.steps)

    @property
    def current(self) -> TourStep | None:
        if self.is_complete:
            return None
        return self.steps[self.current_step]

    def advance(self) -> None:
        self.current_step += 1


# ── Default tour ──────────────────────────────────────────────────────────────

DEFAULT_TOUR = Tour(
    name="welcome",
    steps=[
        TourStep(
            id="welcome",
            trigger="connected",
            message=(
                "[bold cyan]Welcome to the Aster shell![/bold cyan]\n"
                "  Try [green]ls[/green] to see what's here."
            ),
        ),
        TourStep(
            id="after_ls_root",
            trigger="command",
            trigger_value="ls",
            message=(
                "  You can explore [cyan]services/[/cyan], [cyan]blobs/[/cyan], or [cyan]gossip/[/cyan].\n"
                "  Try [green]cd services[/green] to browse available RPC services."
            ),
        ),
        TourStep(
            id="in_services",
            trigger="cd",
            trigger_value="/services",
            message=(
                "  These are the services on this peer.\n"
                "  Try [green]ls[/green] to see them, then [green]cd <ServiceName>[/green] to explore one."
            ),
        ),
        TourStep(
            id="in_service",
            trigger="cd",
            trigger_value="/services/*",
            message=(
                "  You're inside a service. Try [green]ls[/green] to see its methods.\n"
                "  You can invoke a method directly: [green]./methodName arg=value[/green]\n"
                "  Or use [green]describe[/green] to see the full contract."
            ),
        ),
        TourStep(
            id="first_invoke",
            trigger="invoke",
            message=(
                "  Nice! You just made your first RPC call.\n"
                "  [dim]Tip: methods with no args will prompt you interactively.[/dim]\n"
                "  [dim]Use Tab for autocomplete anywhere.[/dim]\n"
                "\n"
                "  [bold]You're all set![/bold] Type [green]help[/green] anytime to see available commands."
            ),
        ),
    ],
)


# ── Guide manager ─────────────────────────────────────────────────────────────

class GuideManager:
    """Manages the guided tour state and event dispatch."""

    def __init__(self, display: Any, tour: Tour | None = None) -> None:
        self._display = display
        self._tour = tour or Tour(name="empty")
        self._enabled = True
        self._listeners: list[Callable[[str, str | None], None]] = []

    @property
    def tour(self) -> Tour:
        return self._tour

    @property
    def is_active(self) -> bool:
        return self._enabled and not self._tour.is_complete

    def disable(self) -> None:
        """Disable the guide (e.g., for experienced users)."""
        self._enabled = False

    def add_listener(self, listener: Callable[[str, str | None], None]) -> None:
        """Add a custom event listener for extensibility."""
        self._listeners.append(listener)

    def fire(self, event: str, value: str | None = None) -> None:
        """Fire a guide event. Shows hint if it matches the current step.

        Args:
            event: Event name (e.g., "command", "cd", "invoke", "connected").
            value: Optional value (e.g., command name, target path).
        """
        if not self._enabled:
            return

        # Notify custom listeners
        for listener in self._listeners:
            listener(event, value)

        step = self._tour.current
        if step is None:
            return

        # Check if this event matches the current step's trigger
        if step.trigger != event:
            return

        # Check trigger_value if specified
        if step.trigger_value is not None:
            if step.trigger_value.endswith("/*"):
                # Glob match: /services/* matches /services/anything
                prefix = step.trigger_value[:-1]
                if value is None or not value.startswith(prefix):
                    return
            elif step.trigger_value != value:
                return

        # Show the hint
        self._display.print()
        self._display.print(f"  [dim]{'─' * 50}[/dim]")
        self._display.print(step.message)
        self._display.print(f"  [dim]{'─' * 50}[/dim]")
        self._display.print()

        self._tour.advance()


# ── Persistence ───────────────────────────────────────────────────────────────

_CONFIG_PATH = Path(os.path.expanduser("~/.aster/config.toml"))


def is_first_time() -> bool:
    """Check if this is the user's first time using the shell."""
    if not _CONFIG_PATH.exists():
        return True

    try:
        if __import__("sys").version_info >= (3, 11):
            import tomllib
        else:
            import tomli as tomllib  # type: ignore[no-redef]

        with _CONFIG_PATH.open("rb") as f:
            config = tomllib.load(f)
        return config.get("shell", {}).get("first_time", True)
    except Exception:
        return True


def mark_tour_complete() -> None:
    """Mark the guided tour as complete in the config."""
    _CONFIG_PATH.parent.mkdir(parents=True, exist_ok=True)

    # Read existing config
    existing_lines: list[str] = []
    if _CONFIG_PATH.exists():
        existing_lines = _CONFIG_PATH.read_text().splitlines()

    # Check if [shell] section exists
    shell_section_idx = None
    first_time_idx = None
    for i, line in enumerate(existing_lines):
        if line.strip() == "[shell]":
            shell_section_idx = i
        if "first_time" in line and shell_section_idx is not None:
            first_time_idx = i

    if first_time_idx is not None:
        existing_lines[first_time_idx] = "first_time = false"
    elif shell_section_idx is not None:
        existing_lines.insert(shell_section_idx + 1, "first_time = false")
    else:
        existing_lines.append("")
        existing_lines.append("[shell]")
        existing_lines.append("first_time = false")

    _CONFIG_PATH.write_text("\n".join(existing_lines) + "\n")
