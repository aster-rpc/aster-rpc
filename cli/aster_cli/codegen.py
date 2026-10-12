"""
aster_cli.codegen -- Moved to the binding as ``aster.codegen.python``.

Kept so the Python CLI (and its TypeScript generator, which shares the type
collection) keeps working until the Rust CLI replaces it.
"""

from aster.codegen.python import (  # noqa: F401
    _TypeRecord,
    _to_snake_case,
    classify_types,
    collect_types,
    format_usage_snippet,
    generate_python_clients,
)
