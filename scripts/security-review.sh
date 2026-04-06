#!/usr/bin/env bash
#
# security-review.sh — Claude-powered security review for git commits.
#
# Usage:
#   As a pre-commit hook:  ln -sf ../../scripts/security-review.sh .git/hooks/pre-commit
#   Manually:              ./scripts/security-review.sh
#
# Analyzes the staged diff for:
#   - Deserialization from untrusted sources without size limits
#   - Missing validation on hex fields, metadata, JSON payloads
#   - New json.loads / bytes.fromhex / codec.decode without bounds checking
#   - Decompression without size limits
#   - Network reads without timeouts
#   - NATIVE mode Fory usage (should be local-only)
#   - Hardcoded secrets, credentials, or keys
#
# Requires: claude CLI (Claude Code) installed and authenticated.
#
set -euo pipefail

# Get the staged diff (or full diff if nothing staged)
DIFF=$(git diff --cached --unified=5 -- '*.py' '*.rs' 2>/dev/null)
if [ -z "$DIFF" ]; then
    DIFF=$(git diff --unified=5 -- '*.py' '*.rs' 2>/dev/null)
fi

if [ -z "$DIFF" ]; then
    echo "No changes to review."
    exit 0
fi

# Check if claude is available
if ! command -v claude &>/dev/null; then
    echo "⚠ Claude CLI not found — skipping security review."
    echo "  Install: https://claude.ai/code"
    exit 0
fi

echo "🔒 Running Claude security review..."

# Pipe the diff to Claude with a focused security prompt
RESULT=$(echo "$DIFF" | claude --print --dangerously-skip-permissions \
    "You are a security auditor reviewing a code diff for the Aster RPC framework.

REVIEW THIS DIFF for the following security issues. Be concise — only report actual findings, not clean code.

CRITICAL (block commit):
- Deserialization of untrusted data without size limits (json.loads on network input without capping list/dict sizes)
- bytes.fromhex() on untrusted input without length validation
- codec.decode() or decompress() without MAX_DECOMPRESSED_SIZE enforcement
- Network reads (read_exact, read_to_end) without timeouts
- Use of SerializationMode.NATIVE with untrusted network input (NATIVE allows arbitrary Python types)
- Hardcoded secrets, private keys, or credentials
- eval(), exec(), __import__(), or pickle usage on untrusted data

HIGH (warn):
- New json.loads() calls without checking the source is trusted
- Missing validate_metadata() on StreamHeader/CallHeader metadata
- Missing validate_hex_field() on credential fields
- Unbounded list/string fields from network peers
- File operations without path traversal checks

If you find issues, output them as:
SECURITY: [CRITICAL|HIGH] file:line — description

If the diff is clean, output:
CLEAN: No security issues found.

Do NOT explain what the code does. Only report findings." 2>/dev/null)

# Check for findings
if echo "$RESULT" | grep -q "^SECURITY: CRITICAL"; then
    echo ""
    echo "❌ CRITICAL security issues found:"
    echo "$RESULT" | grep "^SECURITY:"
    echo ""
    echo "Fix these issues before committing."
    echo "To bypass: git commit --no-verify"
    exit 1
elif echo "$RESULT" | grep -q "^SECURITY: HIGH"; then
    echo ""
    echo "⚠ Security warnings:"
    echo "$RESULT" | grep "^SECURITY:"
    echo ""
    echo "Review these warnings. Committing anyway."
    exit 0
else
    echo "✅ Security review clean."
    exit 0
fi
