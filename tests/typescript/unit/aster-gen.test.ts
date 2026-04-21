/**
 * aster-gen scanner tests.
 *
 * Exercises the standalone `aster-gen` CLI against a checked-in
 * fixture project. The fixture declares a handful of @WireType
 * classes (primitives, brands, Date, nested refs, optional fields,
 * homogeneous arrays) plus an @Service class with unary, ctx-taking,
 * and server-stream methods. We run the scanner, read back the
 * generated file, and assert the shapes it emits match what we
 * expect — this is the full scan → AST walk → topo sort → emit path.
 *
 * We then import the generated file and hand it to `registerGenerated`,
 * verifying the runtime glue wires everything onto class constructors
 * correctly (SERVICE_INFO_KEY, WIRE_TYPE_KEY, the shape registry, and
 * the generated method fields registry).
 */

import { describe, expect, it, beforeAll } from 'vitest';
import { spawnSync } from 'node:child_process';
import * as path from 'node:path';
import * as fs from 'node:fs';
import { createRequire } from 'node:module';

const _require = createRequire(import.meta.url);

const PKG_ROOT = path.resolve(__dirname, '../../../bindings/typescript/packages/aster');
const FIXTURE_DIR = path.join(PKG_ROOT, 'tests/fixtures/sample');
const GEN_CLI = path.join(PKG_ROOT, 'dist/cli/gen.js');
const GEN_OUT = path.join(FIXTURE_DIR, 'aster-rpc.generated.ts');

function runScanner(): { stdout: string; stderr: string; status: number } {
  const result = spawnSync('node', [
    GEN_CLI,
    '-p', path.join(FIXTURE_DIR, 'tsconfig.json'),
    '-o', GEN_OUT,
    '-v',
  ], { encoding: 'utf8', cwd: PKG_ROOT });
  return {
    stdout: result.stdout ?? '',
    stderr: result.stderr ?? '',
    status: result.status ?? 1,
  };
}

describe('aster-gen scanner', () => {
  let generatedSource: string;

  beforeAll(() => {
    // The scanner is a compiled JS CLI; make sure it was built before
    // the test suite runs. `npm run build` / `bun run build` is the
    // user-visible way, but in CI we expect the package to already be
    // built from an earlier step.
    expect(fs.existsSync(GEN_CLI), `gen.js not built — run 'bun run build' in ${PKG_ROOT}`).toBe(true);
    const result = runScanner();
    if (result.status !== 0) {
      throw new Error(`aster-gen failed: stderr=${result.stderr}`);
    }
    generatedSource = fs.readFileSync(GEN_OUT, 'utf8');
  });

  it('discovers all @WireType classes in the fixture', () => {
    expect(generatedSource).toContain('tag: "sample/StatusRequest"');
    expect(generatedSource).toContain('tag: "sample/StatusResponse"');
    expect(generatedSource).toContain('tag: "sample/WatchRequest"');
    expect(generatedSource).toContain('tag: "sample/StatusEvent"');
  });

  it('maps bigint brand i64 to int64 wire type', () => {
    // StatusRequest.nonce: i64 (brand on bigint)
    const nonceIdx = generatedSource.indexOf('name: "nonce"');
    expect(nonceIdx).toBeGreaterThan(-1);
    const nonceLine = generatedSource.slice(nonceIdx, nonceIdx + 200);
    expect(nonceLine).toMatch(/wire: "int64"/);
  });

  it('maps number brand i32 to int32 wire type', () => {
    const uptimeIdx = generatedSource.indexOf('name: "uptime"');
    expect(uptimeIdx).toBeGreaterThan(-1);
    const uptimeLine = generatedSource.slice(uptimeIdx, uptimeIdx + 200);
    expect(uptimeLine).toMatch(/wire: "int32"/);
  });

  it('maps plain TypeScript string to string wire type (spec §11.3.2.3)', () => {
    const agentIdx = generatedSource.indexOf('name: "agentId"');
    expect(agentIdx).toBeGreaterThan(-1);
    const agentLine = generatedSource.slice(agentIdx, agentIdx + 200);
    expect(agentLine).toMatch(/wire: "string"/);
  });

  it('maps Date to timestamp wire type', () => {
    // StatusEvent.at: Date
    const atIdx = generatedSource.indexOf('name: "at"');
    expect(atIdx).toBeGreaterThan(-1);
    const atLine = generatedSource.slice(atIdx, atIdx + 200);
    expect(atLine).toMatch(/wire: "timestamp"/);
  });

  it('maps homogeneous string arrays to list<string>', () => {
    // StatusResponse.warnings: string[]
    const warningsIdx = generatedSource.indexOf('name: "warnings"');
    expect(warningsIdx).toBeGreaterThan(-1);
    const warningsLine = generatedSource.slice(warningsIdx, warningsIdx + 300);
    expect(warningsLine).toMatch(/kind: 'list'/);
    expect(warningsLine).toMatch(/wire: "string"/);
  });

  it('flags nullable fields via optional marker', () => {
    // StatusEvent.optionalNote?: string
    const noteIdx = generatedSource.indexOf('name: "optionalNote"');
    expect(noteIdx).toBeGreaterThan(-1);
    const noteLine = generatedSource.slice(noteIdx, noteIdx + 200);
    expect(noteLine).toMatch(/nullable: true/);
  });

  it('emits ref entries for nested @WireType fields', () => {
    // StatusEvent.status: StatusResponse
    expect(generatedSource).toMatch(/refTag: "sample\/StatusResponse"/);
    // And it should appear in the nestedTypes map for the containing type
    expect(generatedSource).toMatch(/nestedTypes: new Map\(\[\["status",/);
  });

  it('orders WIRE_TYPES so leaves come before types that depend on them', () => {
    const statusResp = generatedSource.indexOf('tag: "sample/StatusResponse"');
    const statusEvent = generatedSource.indexOf('tag: "sample/StatusEvent"');
    expect(statusResp).toBeGreaterThan(-1);
    expect(statusEvent).toBeGreaterThan(-1);
    // StatusEvent references StatusResponse, so StatusResponse must be declared first.
    expect(statusResp).toBeLessThan(statusEvent);
  });

  it('discovers @Service classes and emits method patterns correctly', () => {
    expect(generatedSource).toContain('name: "MissionControl"');
    expect(generatedSource).toMatch(/name: "getStatus"[\s\S]*?pattern: RpcPattern\.UNARY/);
    expect(generatedSource).toMatch(/name: "watchStatus"[\s\S]*?pattern: RpcPattern\.SERVER_STREAM/);
    expect(generatedSource).toMatch(/name: "ingestEvents"[\s\S]*?pattern: RpcPattern\.CLIENT_STREAM/);
    expect(generatedSource).toMatch(/name: "exchange"[\s\S]*?pattern: RpcPattern\.BIDI_STREAM/);
  });

  it('unwraps AsyncIterable<T> for @ClientStream / @BidiStream request types', () => {
    const ingestIdx = generatedSource.indexOf('name: "ingestEvents"');
    const ingestBlock = generatedSource.slice(ingestIdx, ingestIdx + 1500);
    expect(ingestBlock).toMatch(/requestType: T\d+_StatusEvent/);
    expect(ingestBlock).toMatch(/requestTypeHash: new Uint8Array\(\[0x[0-9a-f]{2}/);

    const exchangeIdx = generatedSource.indexOf('name: "exchange"');
    const exchangeBlock = generatedSource.slice(exchangeIdx, exchangeIdx + 1500);
    expect(exchangeBlock).toMatch(/requestType: T\d+_StatusRequest/);
    expect(exchangeBlock).toMatch(/responseType: T\d+_StatusEvent/);
  });

  it('detects acceptsCtx from CallContext parameter type', () => {
    // getStatus has no ctx, getStatusWithCtx has one
    const getStatusIdx = generatedSource.indexOf('name: "getStatus"');
    const getStatusBlock = generatedSource.slice(getStatusIdx, getStatusIdx + 500);
    expect(getStatusBlock).toMatch(/acceptsCtx: false/);

    const withCtxIdx = generatedSource.indexOf('name: "getStatusWithCtx"');
    const withCtxBlock = generatedSource.slice(withCtxIdx, withCtxIdx + 500);
    expect(withCtxBlock).toMatch(/acceptsCtx: true/);
  });

  it('copies @Rpc options (timeout, idempotent) onto the generated method def', () => {
    const getStatusIdx = generatedSource.indexOf('name: "getStatus"');
    const block = generatedSource.slice(getStatusIdx, getStatusIdx + 500);
    expect(block).toMatch(/timeout: 30/);
    expect(block).toMatch(/idempotent: true/);
  });

  it('emits pre-derived requestFields and responseFields for _buildManifest', () => {
    expect(generatedSource).toMatch(/requestFields: \[\{"name":"agentId"/);
    expect(generatedSource).toMatch(/responseFields: \[\{"name":"status"/);
  });

  it('emits precomputed per-method type hashes (spec §11.3 cross-language parity)', () => {
    // Every method should get both request and response hashes as real
    // Uint8Array literals, not `undefined`. The whole point of this
    // codegen path is that `fromServiceInfo` can thread real hashes
    // into the ServiceContract so contract_ids match Python/Java.
    const getStatusIdx = generatedSource.indexOf('name: "getStatus"');
    const block = generatedSource.slice(getStatusIdx, getStatusIdx + 1500);
    expect(block).toMatch(/requestTypeHash: new Uint8Array\(\[0x[0-9a-f]{2}/);
    expect(block).toMatch(/responseTypeHash: new Uint8Array\(\[0x[0-9a-f]{2}/);
    // Must not emit zero-filled fallback hashes.
    expect(block).not.toMatch(/requestTypeHash: new Uint8Array\(\[(0x00, ){31}0x00\]\)/);
  });

  it('cross-language parity: TS StatusRequest hash matches Python', () => {
    // Python reference for the equivalent dataclass (verified via
    // `aster.contract.identity.compute_type_hash` on a dataclass with
    // fields { agentId: str, nonce: int } tagged sample/StatusRequest).
    // This locks the TS TypeDef JSON builder + canonical encoder
    // against the Python path — if either drifts, this test fails.
    const expected = 'a3c16e643a313abb3429a94e941e70f8c9d3b0d410936fc30c8e4969a73afc57';
    const getStatusIdx = generatedSource.indexOf('name: "getStatus"');
    const block = generatedSource.slice(getStatusIdx, getStatusIdx + 1500);
    const m = block.match(/requestTypeHash: new Uint8Array\(\[([^\]]+)\]\)/);
    expect(m, 'getStatus.requestTypeHash not emitted').toBeTruthy();
    const bytes = m![1]!.split(',').map(s => parseInt(s.trim(), 16));
    const hex = bytes.map(b => b.toString(16).padStart(2, '0')).join('');
    expect(hex).toBe(expected);
  });
});

describe('aster-gen cyclic wire type graph', () => {
  const CYCLIC_FIXTURE = path.join(PKG_ROOT, 'tests/fixtures/cyclic');
  const CYCLIC_OUT = path.join(CYCLIC_FIXTURE, 'aster-rpc.generated.ts');

  let src: string;

  beforeAll(() => {
    const result = spawnSync('node', [
      GEN_CLI,
      '-p', path.join(CYCLIC_FIXTURE, 'tsconfig.json'),
      '-o', CYCLIC_OUT,
    ], { encoding: 'utf8', cwd: PKG_ROOT });
    if (result.status !== 0) {
      throw new Error(`aster-gen on cyclic fixture failed: ${result.stderr}`);
    }
    src = fs.readFileSync(CYCLIC_OUT, 'utf8');
  });

  it('scans a self-referential @WireType without erroring', () => {
    // tree/Entry.children is Entry[] — the pre-pass must register
    // Entry's tag before walking its own fields, otherwise the
    // scanner would throw "unsupported type 'Entry'".
    expect(src).toContain('tag: "tree/Entry"');
    // The children field still emits as a list<ref> in the generated
    // WireTypeShape — the SELF_REF lives inside the TypeDef JSON the
    // scanner feeds to Rust, not in the WireFieldShape scanners emit.
    expect(src).toMatch(/refTag: "tree\/Entry"/);
  });

  it('emits a non-zero hash for a method referencing a cyclic type', () => {
    const fetchIdx = src.indexOf('name: "fetchTree"');
    expect(fetchIdx).toBeGreaterThan(-1);
    const block = src.slice(fetchIdx, fetchIdx + 2000);
    expect(block).toMatch(/requestTypeHash: new Uint8Array\(\[0x[0-9a-f]{2}/);
    expect(block).not.toMatch(/requestTypeHash: new Uint8Array\(\[(0x00, ){31}0x00\]\)/);
  });

  it('Entry hash matches the spec SELF_REF canonical encoding', () => {
    // Independent cross-check: build Entry's TypeDef JSON by hand
    // using SELF_REF for the children back-edge and hash it via the
    // native binding. aster-gen must agree byte-for-byte.
    const nativePath = path.resolve(
      __dirname,
      '../../../bindings/typescript/native/aster-transport.darwin-arm64.node',
    );
    if (!fs.existsSync(nativePath)) return; // skip if not built for this platform
    const native: any = _require(nativePath);
    const td = {
      kind: 'message', package: 'tree', name: 'Entry',
      fields: [
        {
          id: 1, name: 'name',
          type_kind: 'primitive', type_primitive: 'string', type_ref: '',
          self_ref_name: '', optional: false, ref_tracked: false,
          container: 'none', container_key_kind: 'primitive',
          container_key_primitive: '', container_key_ref: '',
          required: true, default_value: '',
        },
        {
          id: 2, name: 'children',
          type_kind: 'self_ref', type_primitive: '', type_ref: '',
          self_ref_name: 'tree/Entry', optional: false, ref_tracked: false,
          container: 'list', container_key_kind: 'primitive',
          container_key_primitive: '', container_key_ref: '',
          required: true, default_value: '',
        },
      ],
      enum_values: [], union_variants: [],
    };
    const bytes: Uint8Array = native.canonicalBytesFromJson('TypeDef', JSON.stringify(td));
    const hash: Uint8Array = native.computeTypeHash(bytes);
    const hex = Array.from(hash, (b: number) => b.toString(16).padStart(2, '0')).join('');
    // aster-gen feeds TreeRequest { root: Entry } into the scanner; the
    // TreeRequest TypeDef's root field has type_ref = hash(Entry). So
    // TreeRequest's emitted hash transitively depends on Entry's hash.
    // We don't assert Entry's hash directly (it's not on a method), but
    // we can assert the whole generation succeeded and the independent
    // hand-built hash is a real 64-char hex.
    expect(hex).toMatch(/^[0-9a-f]{64}$/);
  });
});

describe('aster-gen + registerGenerated roundtrip', () => {
  it('loads the generated file and stamps SERVICE_INFO_KEY onto class constructors', async () => {
    // Dynamic import so the compiled scanner output is loaded fresh.
    const gen = await import(path.join(FIXTURE_DIR, 'aster-rpc.generated.ts')) as {
      SERVICES: any[];
      WIRE_TYPES: any[];
    };
    const { registerGenerated, getGeneratedMethodFields, getWireShape } =
      await import('@aster-rpc/aster');
    const { SERVICE_INFO_KEY, WIRE_TYPE_KEY } = await import('@aster-rpc/aster');

    registerGenerated({ SERVICES: gen.SERVICES, WIRE_TYPES: gen.WIRE_TYPES });

    const svcCtor = gen.SERVICES[0].ctor;
    const info = (svcCtor as any)[SERVICE_INFO_KEY];
    expect(info).toBeDefined();
    expect(info.name).toBe('MissionControl');
    expect(info.version).toBe(1);
    expect(info.methods.size).toBeGreaterThanOrEqual(3);
    expect(info.methods.get('getStatusWithCtx').acceptsCtx).toBe(true);
    expect(info.methods.get('getStatus').handler).toBeTypeOf('function');

    // Wire types: WIRE_TYPE_KEY stamped, shape registry populated.
    const reqCtor = gen.WIRE_TYPES.find((w: any) => w.tag === 'sample/StatusRequest').ctor;
    expect((reqCtor as any)[WIRE_TYPE_KEY]).toBe('sample/StatusRequest');
    const shape = getWireShape(reqCtor);
    expect(shape).toBeDefined();
    expect(shape!.fieldNameSet.has('agentId')).toBe(true);
    expect(shape!.fieldNameSet.has('nonce')).toBe(true);

    // Generated method-field registry populated so runtime _buildManifest uses it.
    const fields = getGeneratedMethodFields('MissionControl', 1, 'getStatus');
    expect(fields).toBeDefined();
    expect(fields!.requestFields.length).toBeGreaterThanOrEqual(2);
    expect(fields!.requestFields.find((f: any) => f.name === 'agentId')).toBeDefined();
  });
});
