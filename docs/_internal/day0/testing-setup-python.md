# Day 0 Testing Setup

**Date:** 2026-04-09

## Goal

An agent (human or LLM) follows the Mission Control GUIDE.md in a clean
environment and reports what breaks. No dev dependencies, no existing
config, no shared state with the developer's machine.

## Part 1: What you (the developer) set up

### 1a. Build fresh wheels

```bash

cd /Users/emrul/dev/emrul/iroh-python
./scripts/build.sh

```

This produces:
- `bindings/python/target/wheels/aster_rpc-*.whl` (the framework)
- `cli/` (the CLI package, installed as editable or from source)

### 1b. Create the isolated test environment

```bash

# Create a temp home so ~/.aster/ is isolated
export DAY0_HOME=$(mktemp -d)
export HOME=$DAY0_HOME
export XDG_CONFIG_HOME=$DAY0_HOME/.config

# Create and activate a clean venv
uv venv $DAY0_HOME/venv
source $DAY0_HOME/venv/bin/activate


cp examples/mission-control/GUIDE.md $DAY0_HOME/GUIDE.md
cp docs/_internal/day0/testing-instructions.md $DAY0_HOME/testing-instructions.md
cd $DAY0_HOME

# Install the framework and CLI from local wheels
uv pip install /Users/emrul/dev/emrul/iroh-python/bindings/python/target/wheels/aster_rpc-*.whl
uv pip install /Users/emrul/dev/emrul/iroh-python/cli/

# Verify clean state
echo "Home: $HOME"
ls -la ~/.aster 2>/dev/null || echo "Clean — no .aster directory"
which aster
aster --help

```

### 1c. Verify the install

```bash
python -c "from aster import AsterServer, AsterClient, service, rpc, wire_type; print('Import OK')"
aster --help
```

Both should succeed. If either fails, the wheel or CLI install is broken.

### 1d. Cleanup (after testing)

```bash

source deactivate
rm -rf $DAY0_HOME
unset DAY0_HOME
export HOME=~  # restore

```

```bash

minimax --dangerously-skip-permissions --allow-dangerously-skip-permissions \
    --system-prompt "You are a QA tester. Your working directory is $HOME. All files you need are here. Do NOT explore other directories.
      Follow testing-instructions.md against GUIDE.md. Create all code files in ~/day0-test/. Use background processes for the server. Report
      PASS/FAIL for each checklist item." \
    "Read testing-instructions.md and GUIDE.md in the current directory, then execute the test plan chapter by chapter. If something doesn't work, log and move on."

```

Instructions for agent in [testing-instructions.md](testing-instructions.md)