import pytest

def pytest_configure(config):
    config.addinivalue_line("markers", "chaos: Jepsen-inspired chaos/fault-injection tests")
