/**
 * @aster-rpc/transport — native loader.
 *
 * Selects the right platform-specific package and re-exports its NAPI bindings.
 *
 * Each platform package (e.g. @aster-rpc/transport-darwin-arm64) contains
 * exactly one prebuilt `.node` file. We're listed as `optionalDependencies`
 * on all platform packages, so npm only installs the matching one for the
 * user's OS/arch and silently skips the others.
 *
 * The loader also supports a local `.node` file in this package's directory
 * (alongside `index.js`) so that monorepo development and CI dry-runs work
 * without requiring all platform packages to be published.
 */

'use strict';

const { join } = require('node:path');
const { existsSync } = require('node:fs');

const { platform, arch } = process;

let nativeBinding = null;
let lastError = null;

/** Map of process.platform/process.arch → suffix used by `napi build --platform`. */
const PLATFORM_TARGETS = {
  'darwin-arm64':       { pkg: '@aster-rpc/transport-darwin-arm64',     local: 'aster-transport.darwin-arm64.node' },
  'darwin-x64':         { pkg: '@aster-rpc/transport-darwin-x64',       local: 'aster-transport.darwin-x64.node' },
  'linux-x64':          { pkg: '@aster-rpc/transport-linux-x64-gnu',    local: 'aster-transport.linux-x64-gnu.node' },
  'linux-arm64':        { pkg: '@aster-rpc/transport-linux-arm64-gnu',  local: 'aster-transport.linux-arm64-gnu.node' },
  'win32-x64':          { pkg: '@aster-rpc/transport-win32-x64-msvc',   local: 'aster-transport.win32-x64-msvc.node' },
};

const target = PLATFORM_TARGETS[`${platform}-${arch}`];
if (!target) {
  throw new Error(
    `@aster-rpc/transport: unsupported platform ${platform}-${arch}.\n` +
    `Supported platforms: ${Object.keys(PLATFORM_TARGETS).join(', ')}.\n` +
    `Open an issue at https://github.com/aster-rpc/aster-rpc/issues to request a new target.`,
  );
}

// 1. Prefer a local .node file in this package directory (monorepo dev,
//    fresh builds, CI dry-runs). This is also where the binary lives in
//    the published platform package once npm extracts it.
const localPath = join(__dirname, target.local);
if (existsSync(localPath)) {
  try {
    nativeBinding = require(localPath);
  } catch (err) {
    lastError = err;
  }
}

// 2. Fall back to the published platform package.
if (!nativeBinding) {
  try {
    nativeBinding = require(target.pkg);
  } catch (err) {
    lastError = err;
  }
}

if (!nativeBinding) {
  throw new Error(
    `@aster-rpc/transport: failed to load native binding for ${platform}-${arch}.\n` +
    `Tried:\n` +
    `  - ${localPath}\n` +
    `  - ${target.pkg}\n` +
    `Last error: ${lastError ? lastError.message : '(unknown)'}\n` +
    `Make sure '${target.pkg}' is installed (it should be a transitive dependency).`,
  );
}

module.exports = nativeBinding;
