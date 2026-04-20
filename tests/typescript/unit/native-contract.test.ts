/**
 * Integration test: contract identity via real NAPI-RS Rust core.
 *
 * Validates golden vectors from the spec (Appendix B) against the
 * actual Rust implementation. Skips if the native addon is not built.
 *
 * Build first: cd native && npx napi build --release --platform
 */

import { describe, it, expect, beforeAll } from 'vitest';
import { resolve, dirname } from 'node:path';
import { existsSync } from 'node:fs';
import { createRequire } from 'node:module';
import { fileURLToPath } from 'node:url';

const __dirname = dirname(fileURLToPath(import.meta.url));
const require = createRequire(import.meta.url);

let native: any;

// Load synchronously at module level so skipIf works
const candidates = [
  resolve(__dirname, '../../../bindings/typescript/native/aster-transport.darwin-arm64.node'),
  resolve(__dirname, '../../../bindings/typescript/native/aster-transport.darwin-x64.node'),
  resolve(__dirname, '../../../bindings/typescript/native/aster-transport.linux-x64-gnu.node'),
  resolve(__dirname, '../../../bindings/typescript/native/aster-transport.linux-arm64-gnu.node'),
];

for (const path of candidates) {
  if (existsSync(path)) {
    try {
      native = require(path);
      break;
    } catch {
      // try next
    }
  }
}

const available = !!native;
if (!available) {
  console.warn('Native addon not found. Run: cd native && npx napi build --release --platform');
}

function hex(data: Uint8Array | Buffer): string {
  return Array.from(new Uint8Array(data), b => b.toString(16).padStart(2, '0')).join('');
}

describe('native contract identity (Rust core via NAPI)', () => {
  it.skipIf(!available)('version() returns crate version', () => {
    expect(native.version()).toBe('0.2.0');
  });

  it.skipIf(!available)('golden vector 1: Echo — canonical bytes', () => {
    const json = JSON.stringify({
      name: "Echo", version: 1,
      methods: [{ name: "echo", pattern: "unary",
        request_type: "0".repeat(64), response_type: "0".repeat(64),
        idempotent: false, default_timeout: 0.0, requires: null }],
      serialization_modes: [], scoped: "shared", requires: null,
    });
    const bytes = native.canonicalBytesFromJson('ServiceContract', json);
    expect(hex(bytes)).toBe(
      '124563686f02010c126563686f00200000000000000000000000000000000000000000000000000000000000000000200000000000000000000000000000000000000000000000000000000000000000000000000000000000fd000c00fd02'
    );
    expect(bytes.length).toBe(95);
  });

  it.skipIf(!available)('golden vector 1: Echo — BLAKE3 hash', () => {
    const json = JSON.stringify({
      name: "Echo", version: 1,
      methods: [{ name: "echo", pattern: "unary",
        request_type: "0".repeat(64), response_type: "0".repeat(64),
        idempotent: false, default_timeout: 0.0, requires: null }],
      serialization_modes: [], scoped: "shared", requires: null,
    });
    expect(native.computeContractIdFromJson(json)).toBe(
      'ac4495d03d0fe1ee8249e725b8aff0b66fdaecc0e76b27e26149225d1ccba68f'
    );
  });

  it.skipIf(!available)('golden vector 2: DataService with capability', () => {
    const json = JSON.stringify({
      name: "DataService", version: 1,
      methods: [{ name: "get_record", pattern: "unary",
        request_type: "0".repeat(64), response_type: "0".repeat(64),
        idempotent: true, default_timeout: 30000.0,
        requires: { kind: "any_of", roles: ["reader", "ai-reader"] }}],
      serialization_modes: [], scoped: "shared", requires: null,
    });
    expect(native.computeContractIdFromJson(json)).toBe(
      'bb4e859ac2a8d9865f4c0fa7766c0197fdf830d925ca7753a34fe0ffe757c20e'
    );
  });

  it.skipIf(!available)('golden vector 3: Analytics multi-method', () => {
    const json = JSON.stringify({
      name: "Analytics", version: 2,
      methods: [
        { name: "query", pattern: "unary", request_type: "0".repeat(64), response_type: "0".repeat(64), idempotent: true, default_timeout: 0.0, requires: null },
        { name: "watch", pattern: "server_stream", request_type: "0".repeat(64), response_type: "0".repeat(64), idempotent: false, default_timeout: 60000.0, requires: null },
        { name: "upload", pattern: "client_stream", request_type: "0".repeat(64), response_type: "0".repeat(64), idempotent: false, default_timeout: 0.0, requires: null },
      ],
      serialization_modes: [], scoped: "shared", requires: null,
    });
    expect(native.computeContractIdFromJson(json)).toBe(
      '9b37cf43f3135e50905f8d9c0feed7530c06e7f7190c50f072e07f76116b5e05'
    );
  });

  it.skipIf(!available)('golden vector 4: ChatRoom session-scoped', () => {
    const json = JSON.stringify({
      name: "ChatRoom", version: 1,
      methods: [{ name: "send_message", pattern: "unary",
        request_type: "0".repeat(64), response_type: "0".repeat(64),
        idempotent: false, default_timeout: 5000.0, requires: null }],
      serialization_modes: [], scoped: "session", requires: null,
    });
    expect(native.computeContractIdFromJson(json)).toBe(
      '3f767eaa700b1c92ea6098b32d8c98e2f4ccabe1b6e614c459660abb366fa847'
    );
  });

  it.skipIf(!available)('encodeFrameNative + decodeFrameNative roundtrip', () => {
    const payload = Buffer.from('Hello, Aster!');
    const encoded = native.encodeFrameNative(payload, 0x04);
    const decoded = native.decodeFrameNative(encoded);
    expect(decoded.flags).toBe(0x04);
    expect(Buffer.from(decoded.payload).toString()).toBe('Hello, Aster!');
  });

  it.skipIf(!available)('computeTypeHash returns 32-byte BLAKE3', () => {
    const hash = native.computeTypeHash(Buffer.from('test'));
    expect(hash.length).toBe(32);
  });

  it.skipIf(!available)('different versions produce different contract IDs', () => {
    const makeJson = (version: number) => JSON.stringify({
      name: "UserService", version,
      methods: [{ name: "get_user", pattern: "unary",
        request_type: "0".repeat(64), response_type: "0".repeat(64),
        idempotent: true, default_timeout: 0.0, requires: null }],
      serialization_modes: [], scoped: "shared", requires: null,
    });
    const h1 = native.computeContractIdFromJson(makeJson(1));
    const h2 = native.computeContractIdFromJson(makeJson(2));
    expect(h1).not.toBe(h2);
  });
});
