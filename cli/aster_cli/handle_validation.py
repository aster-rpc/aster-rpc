"""Client-side Aster handle validation."""

from __future__ import annotations

import re

_HANDLE_RE = re.compile(r"^[a-z0-9](?:[a-z0-9-]{1,37}[a-z0-9])?$")

_RESERVED = {
    "about", "abuse", "access", "account", "accounts", "admin",
    "administrator", "api", "aster", "audit", "billing", "blog", "cache",
    "changelog", "cloud", "codespace", "config", "contact", "control",
    "contribute", "dashboard", "daemon", "developer", "developers",
    "discover", "docs", "documentation", "download", "downloads", "email",
    "enroll", "enterprise", "explore", "features", "feedback", "forum",
    "gateway", "gist", "graphql", "guide", "guides", "help", "home",
    "hosting", "import", "integrations", "issues", "jobs", "learn",
    "legal", "login", "logout", "marketplace", "members", "mention",
    "mentions", "messages", "mobile", "monitor", "new", "no-reply",
    "noreply", "none", "notifications", "null", "oauth", "offer",
    "offers", "official", "opensource", "operator", "organization",
    "organizations", "orgs", "owner", "pages", "password", "payment",
    "payments", "platform", "plan", "plans", "popular", "postmaster",
    "pricing", "privacy", "profile", "project", "projects", "proxy",
    "publish", "readme", "recover", "registry", "relay", "releases",
    "render", "replies", "report", "reports", "root", "search", "security",
    "service", "services", "settings", "setup", "shell", "shop", "signin",
    "signout", "signup", "site", "sitemap", "sponsors", "staff", "status",
    "store", "stories", "suggestions", "support", "system", "team", "teams",
    "terms", "topics", "training", "trending", "trust", "undefined",
    "user", "users", "verify", "webmaster", "welcome", "wiki", "www",
}


def validate_handle(handle: str) -> tuple[bool, str]:
    """Return ``(ok, reason)`` for a proposed handle."""
    if not handle:
        return False, "handle is required"
    if handle != handle.lower():
        return False, "handle must be lowercase"
    if len(handle) < 3 or len(handle) > 39:
        return False, "handle must be 3-39 characters"
    if handle.isdigit():
        return False, "handle cannot be purely numeric"
    if "--" in handle:
        return False, "handle cannot contain consecutive hyphens"
    if handle.startswith("aster-") or handle.startswith("admin-"):
        return False, "handle prefix is reserved"
    if handle in _RESERVED:
        return False, "handle is reserved"
    if not _HANDLE_RE.match(handle):
        return False, "use lowercase letters, numbers, and single hyphens only"
    return True, "available"
