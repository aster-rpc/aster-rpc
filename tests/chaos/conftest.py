import sys
from pathlib import Path

import pytest

# The wait budget lives with the main suite (tests/python/wait_budget.py).
sys.path.insert(0, str(Path(__file__).resolve().parent.parent / "python"))


def pytest_configure(config):
    config.addinivalue_line("markers", "chaos: Jepsen-inspired chaos/fault-injection tests")
