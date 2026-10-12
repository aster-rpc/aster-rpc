"""
aster.codegen -- Typed client generation from contract manifests.

``python -m aster.codegen client …`` is the handoff the ``aster`` CLI calls.
"""

from aster.codegen.python import format_usage_snippet, generate_python_clients

__all__ = ["format_usage_snippet", "generate_python_clients"]
