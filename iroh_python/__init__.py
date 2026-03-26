"""
iroh_python - Python bindings for the Iroh P2P networking library.

This package provides Python access to Iroh's peer-to-peer networking
capabilities, including QUIC connections, content-addressed blob storage,
collaborative CRDT documents, and topic-based gossip messaging.
"""

# Import native extension module
try:
    from iroh_python._iroh_python import *
except ImportError as e:
    # Provide helpful error if native module not built
    raise ImportError(
        "Could not import native extension module. "
        "Please build the extension with 'maturin develop' first."
    ) from e

__version__ = "0.1.0"

# __all__ will be populated as modules are implemented
__all__ = []