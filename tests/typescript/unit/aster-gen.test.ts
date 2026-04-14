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

const PKG_ROOT = path.resolve(__dirname, '../../../bindings/typescript/packages/aster');
const FIXTURE_DIR = path.join(PKG_ROOT, 'tests/fixtures/sample');
const GEN_CLI = path.join(PKG_ROOT, 'dist/cli/gen.js');
const GEN_OUT = path.join(FIXTURE_DIR, 'rpc.generated.ts');

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
});

describe('aster-gen + registerGenerated roundtrip', () => {
  it('loads the generated file and stamps SERVICE_INFO_KEY onto class constructors', async () => {
    // Dynamic import so the compiled scanner output is loaded fresh.
    const gen = await import(path.join(FIXTURE_DIR, 'rpc.generated.ts')) as {
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
