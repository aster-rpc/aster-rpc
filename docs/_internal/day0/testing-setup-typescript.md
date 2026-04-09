# Day 0 Testing Setup (TypeScript)

**Date:** 2026-04-10

## Goal

An agent (human or LLM) follows the Mission Control GUIDE_TypeScript.md
in a clean environment and reports what breaks. No dev dependencies, no
existing config, no shared state with the developer's machine.

## Part 1: What you (the developer) set up

### 1a. Build fresh Python wheels (needed for CLI tools)

```bash
cd /Users/emrul/dev/emrul/iroh-python
./scripts/build.sh
```

This produces:
- `bindings/python/target/wheels/aster_rpc-*.whl` (the framework)
- `cli/` (the CLI package)

### 1b. Build the TS native addon

```bash

cd /Users/emrul/dev/emrul/iroh-python/bindings/typescript/native
npx napi build --release --platform

```

This produces a platform-specific `.node` file (e.g.,
`aster-transport.darwin-arm64.node`).

### 1c. Build the TS packages

```bash

cd /Users/emrul/dev/emrul/iroh-python/bindings/typescript
bun install --ignore-scripts
npx tsc -p packages/aster/tsconfig.json

```

### 1d. Create the isolated test environment

```bash

# Create a temp home so ~/.aster/ is isolated
export DAY0_HOME=$(mktemp -d)
export HOME=$DAY0_HOME
export XDG_CONFIG_HOME=$DAY0_HOME/.config

# Copy the guide and test instructions
cp /Users/emrul/dev/emrul/iroh-python/examples/mission-control/GUIDE_TypeScript.md $DAY0_HOME/GUIDE.md
cp /Users/emrul/dev/emrul/iroh-python/docs/_internal/day0/testing-instructions-typescript.md $DAY0_HOME/testing-instructions.md
cd $DAY0_HOME

# Set up TS project with symlinked packages (no bun install needed)
echo '{"name":"day0-ts-test","type":"module"}' > package.json
mkdir -p node_modules/@aster-rpc
ln -sf /Users/emrul/dev/emrul/iroh-python/bindings/typescript/packages/aster node_modules/@aster-rpc/aster
ln -sf /Users/emrul/dev/emrul/iroh-python/bindings/typescript/native node_modules/@aster-rpc/transport

# Set up Python venv for CLI tools (aster shell, aster call, etc.)
uv venv $DAY0_HOME/pyvenv --python 3.13
source $DAY0_HOME/pyvenv/bin/activate
uv pip install /Users/emrul/dev/emrul/iroh-python/bindings/python/target/wheels/aster_rpc-*.whl
uv pip install /Users/emrul/dev/emrul/iroh-python/cli/

# Verify clean state
echo "Home: $HOME"
ls -la ~/.aster 2>/dev/null || echo "Clean -- no .aster directory"
bun --version
which aster    # should show $DAY0_HOME/pyvenv/bin/aster

```

### 1e. Verify the install

```bash

# TS framework
bun -e "import { AsterServer, Service, Rpc, WireType } from '@aster-rpc/aster'; console.log('TS Import OK');"

# Python CLI (must be in activated pyvenv)
source $DAY0_HOME/pyvenv/bin/activate
aster --help

```

Both should succeed. If the TS import fails, the native addon or
symlinks are broken. If `aster --help` fails, the Python wheel or
CLI install is broken.

### 1f. Cleanup (after testing)

```bash

deactivate
rm -rf $DAY0_HOME
unset DAY0_HOME
export HOME=~  # restore

```

### 1g. Run the test agent

```bash

cd $DAY0_HOME
source $DAY0_HOME/pyvenv/bin/activate

minimax --dangerously-skip-permissions \
    --bare \
    --system-prompt "You are a QA tester. Your working directory is $HOME. All files you need are here. Do NOT explore other directories.
      Follow testing-instructions.md against GUIDE.md. Create all code files in ~/day0-test/. Use background processes for the server.
      Use 'bun run' to execute TypeScript files. Use 'aster shell' and 'aster call' from the CLI for client testing.
      The Python venv is already activated (aster CLI is available).
      Report PASS/FAIL for each checklist item." \
    "Read testing-instructions.md and GUIDE.md in the current directory, then execute the test plan chapter by chapter. If something doesn't work, log and move on."

```

Instructions for agent in [testing-instructions-typescript.md](testing-instructions-typescript.md)
