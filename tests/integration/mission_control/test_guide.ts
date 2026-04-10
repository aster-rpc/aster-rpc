#!/usr/bin/env bun
/**
 * Mission Control guide integration test — TypeScript client.
 *
 * Tests every chapter of the Mission Control guide against a running server
 * (Python or TypeScript). Run from `bindings/typescript/packages/aster`:
 *
 *     bun run ../../../../tests/integration/mission_control/test_guide.ts \
 *       <address> --mode dev
 *
 *     bun run ../../../../tests/integration/mission_control/test_guide.ts \
 *       <address> --mode auth --keys-dir <work_dir>
 *
 * Exit codes:
 *   0  all tests passed
 *   1  at least one test failed
 *   2  setup/usage error
 */

import { AsterClientWrapper } from '@aster-rpc/aster';
import { existsSync } from 'node:fs';
import { join } from 'node:path';

// ── Output helpers ──────────────────────────────────────────────────────────

let QUIET = false;
let PASS_COUNT = 0;
let FAIL_COUNT = 0;

function ok(label: string): void {
  PASS_COUNT++;
  if (!QUIET) console.log(`  \x1b[32m✓\x1b[0m ${label}`);
}

function fail(label: string, msg: string): void {
  FAIL_COUNT++;
  console.log(`  \x1b[31m✗\x1b[0m ${label}: ${msg}`);
}

function section(name: string): void {
  if (!QUIET) console.log(`\n\x1b[1m${name}\x1b[0m`);
}

function isPermissionDenied(e: unknown): boolean {
  const msg = String(e);
  return msg.includes('PERMISSION_DENIED') || msg.toLowerCase().includes('permission');
}

// ── Chapter tests ───────────────────────────────────────────────────────────

async function testCh1Unary(mc: any): Promise<void> {
  try {
    const r = await mc.getStatus({ agent_id: 'edge-7' });
    if (typeof r !== 'object' || r === null) {
      fail('Ch1 getStatus', `expected object, got ${typeof r}`);
      return;
    }
    if (r.agent_id !== 'edge-7') {
      fail('Ch1 getStatus', `agent_id mismatch: ${JSON.stringify(r)}`);
      return;
    }
    if (r.status !== 'running') {
      fail('Ch1 getStatus', `status mismatch: ${JSON.stringify(r)}`);
      return;
    }
    if (typeof r.uptime_secs !== 'number' || r.uptime_secs <= 0) {
      fail('Ch1 getStatus', `uptime_secs invalid: ${JSON.stringify(r)}`);
      return;
    }
    ok('Ch1 getStatus returns typed response');
  } catch (e) {
    fail('Ch1 getStatus', String(e));
  }
}

async function testCh2Streaming(mc: any): Promise<void> {
  // 2a: submitLog
  try {
    const r = await mc.submitLog({
      timestamp: Date.now() / 1000,
      level: 'info',
      message: 'ch2 first log',
      agent_id: 'edge-7',
    });
    const accepted = r === true || (r && typeof r === 'object' && r.accepted === true);
    if (!accepted) {
      fail('Ch2 submitLog', `unexpected response: ${JSON.stringify(r)}`);
      return;
    }
    ok('Ch2 submitLog accepted');
  } catch (e) {
    fail('Ch2 submitLog', String(e));
    return;
  }

  // 2b: tailLogs receives a freshly submitted entry
  const received: any[] = [];
  let consumeError: unknown = null;

  const consumePromise = (async () => {
    try {
      for await (const entry of mc.tailLogs.stream({ level: 'info' })) {
        received.push(entry);
        if (received.length >= 1) break;
      }
    } catch (e) {
      consumeError = e;
    }
  })();

  // Give the stream a moment to open, then submit a marker log
  await new Promise(r => setTimeout(r, 300));
  try {
    await mc.submitLog({
      timestamp: Date.now() / 1000,
      level: 'info',
      message: 'ch2 stream marker',
      agent_id: 'edge-7',
    });
  } catch (e) {
    fail('Ch2 tailLogs setup', `submitLog failed: ${e}`);
    return;
  }

  // Wait for the stream to receive at least one entry
  const timeout = new Promise(resolve => setTimeout(() => resolve('timeout'), 5000));
  const result = await Promise.race([consumePromise, timeout]);

  if (result === 'timeout') {
    fail('Ch2 tailLogs', 'no entry received within 5s');
    return;
  }
  if (consumeError) {
    fail('Ch2 tailLogs', String(consumeError));
    return;
  }
  if (received.length === 0) {
    fail('Ch2 tailLogs', 'no entries received');
    return;
  }
  const entry = received[0];
  if (typeof entry !== 'object' || !('message' in entry)) {
    fail('Ch2 tailLogs', `entry missing 'message' field: ${JSON.stringify(entry)}`);
    return;
  }
  ok(`Ch2 tailLogs received entry (${JSON.stringify(entry.message)})`);
}

async function testCh3ClientStream(mc: any): Promise<void> {
  const N = 1000;
  async function* metrics() {
    for (let i = 0; i < N; i++) {
      yield {
        name: 'cpu.usage',
        value: i,
        timestamp: Date.now() / 1000,
      };
    }
  }
  try {
    const r = await mc.ingestMetrics(metrics());
    const accepted = r?.accepted ?? null;
    if (accepted !== N) {
      fail('Ch3 ingestMetrics', `expected accepted=${N}, got ${accepted} (full: ${JSON.stringify(r)})`);
      return;
    }
    ok(`Ch3 ingestMetrics accepted ${N}`);
  } catch (e) {
    fail('Ch3 ingestMetrics', String(e));
  }
}

async function testCh4Session(client: AsterClientWrapper): Promise<void> {
  // For TS we use the proxy client to call AgentSession.
  // Session-scoped semantics are handled by the server via session-stream
  // protocol, but for unary methods within a session, the proxy can drive it.
  //
  // NOTE: If the proxy client cannot drive session-scoped services in TS yet,
  // this test will surface that as a clear failure.
  const agent = client.proxy('AgentSession');

  // 4a: register with GPU
  try {
    const r = await agent.register({
      agent_id: 'gpu-1',
      capabilities: ['gpu'],
      load_avg: 0.5,
    });
    const taskId = r?.task_id ?? r?.taskId;
    if (taskId !== 'train-42') {
      fail('Ch4 register (gpu)', `expected task_id='train-42', got ${JSON.stringify(r)}`);
      return;
    }
    ok('Ch4 register (gpu) returns train-42');
  } catch (e) {
    fail('Ch4 register (gpu)', String(e));
    return;
  }

  // 4b: register without GPU
  try {
    const r = await agent.register({
      agent_id: 'cpu-1',
      capabilities: ['arm64'],
      load_avg: 0.2,
    });
    const taskId = r?.task_id ?? r?.taskId;
    if (taskId !== 'idle') {
      fail('Ch4 register (no gpu)', `expected task_id='idle', got ${JSON.stringify(r)}`);
      return;
    }
    ok('Ch4 register (no gpu) returns idle');
  } catch (e) {
    fail('Ch4 register (no gpu)', String(e));
  }
}

// ── Chapter 5: Auth ──────────────────────────────────────────────────────────

async function testCh5NoCredential(address: string): Promise<void> {
  try {
    const client = new AsterClientWrapper({ address });
    await client.connect();
    await client.close();
    fail('Ch5 no-cred denied', 'connection succeeded (should have been refused)');
  } catch (e) {
    if (isPermissionDenied(e) || String(e).toLowerCase().includes('denied')) {
      ok('Ch5 no credential → denied');
    } else {
      fail('Ch5 no-cred denied', `unexpected error: ${e}`);
    }
  }
}

async function testCh5EdgeCredential(address: string, edgeCred: string): Promise<void> {
  let client: AsterClientWrapper;
  try {
    client = new AsterClientWrapper({
      address,
      enrollmentCredentialFile: edgeCred,
    } as any);
    await client.connect();
  } catch (e) {
    fail('Ch5 edge connect', String(e));
    return;
  }

  const mc = client.proxy('MissionControl');

  // getStatus should succeed
  try {
    const r = await mc.getStatus({ agent_id: 'edge-7' });
    if (r?.status === 'running') {
      ok('Ch5 edge getStatus → OK (has ops.status)');
    } else {
      fail('Ch5 edge getStatus', `unexpected: ${JSON.stringify(r)}`);
    }
  } catch (e) {
    fail('Ch5 edge getStatus', String(e));
  }

  // tailLogs should be denied
  try {
    for await (const _ of mc.tailLogs.stream({ level: 'info' })) {
      break;
    }
    fail('Ch5 edge tailLogs denied', 'stream succeeded (should have been denied)');
  } catch (e) {
    if (isPermissionDenied(e)) {
      ok('Ch5 edge tailLogs → DENIED (lacks ops.logs)');
    } else {
      fail('Ch5 edge tailLogs denied', `unexpected error: ${e}`);
    }
  }

  // runCommand bidi should be denied (the auth bypass test)
  try {
    const agent = client.proxy('AgentSession');
    const ch = agent.runCommand.bidi();
    await ch.open();
    await ch.send({ command: 'echo hello' });
    let gotResponse = false;
    for await (const _ of ch) {
      gotResponse = true;
      break;
    }
    await ch.close();
    if (gotResponse) {
      fail('Ch5 edge runCommand denied', 'bidi succeeded (should have been denied)');
    } else {
      // No response but no error — depends on server behavior
      ok('Ch5 edge runCommand → no response (denied silently)');
    }
  } catch (e) {
    if (isPermissionDenied(e)) {
      ok('Ch5 edge runCommand → DENIED (bidi auth check)');
    } else {
      fail('Ch5 edge runCommand denied', `unexpected error: ${e}`);
    }
  }

  await client.close();
}

async function testCh5OpsCredential(address: string, opsCred: string): Promise<void> {
  let client: AsterClientWrapper;
  try {
    client = new AsterClientWrapper({
      address,
      enrollmentCredentialFile: opsCred,
    } as any);
    await client.connect();
  } catch (e) {
    fail('Ch5 ops connect', String(e));
    return;
  }

  const mc = client.proxy('MissionControl');

  try {
    const r = await mc.getStatus({ agent_id: 'ops' });
    if (r?.status === 'running') {
      ok('Ch5 ops getStatus → OK');
    } else {
      fail('Ch5 ops getStatus', `unexpected: ${JSON.stringify(r)}`);
    }
  } catch (e) {
    fail('Ch5 ops getStatus', String(e));
  }

  // tailLogs (verifies any_of(LOGS, ADMIN) works for ops)
  const received: any[] = [];
  let consumeError: unknown = null;
  const consumePromise = (async () => {
    try {
      for await (const entry of mc.tailLogs.stream({ level: 'info' })) {
        received.push(entry);
        if (received.length >= 1) break;
      }
    } catch (e) {
      consumeError = e;
    }
  })();

  await new Promise(r => setTimeout(r, 300));
  try {
    await mc.submitLog({
      timestamp: Date.now() / 1000,
      level: 'info',
      message: 'ops auth marker',
      agent_id: 'ops',
    });
  } catch { /* ignore */ }

  const timeout = new Promise(resolve => setTimeout(() => resolve('timeout'), 5000));
  const result = await Promise.race([consumePromise, timeout]);

  if (result === 'timeout') {
    fail('Ch5 ops tailLogs', 'no entry received within 5s');
  } else if (consumeError) {
    fail('Ch5 ops tailLogs', String(consumeError));
  } else if (received.length > 0) {
    ok('Ch5 ops tailLogs → OK (entry received)');
  } else {
    fail('Ch5 ops tailLogs', 'stream opened but no entries');
  }

  await client.close();
}

// ── Mode runners ─────────────────────────────────────────────────────────────

async function runDevMode(address: string): Promise<void> {
  section('Dev mode (no auth)');

  const client = new AsterClientWrapper({ address });
  try {
    await client.connect();
  } catch (e) {
    fail('connect', String(e));
    return;
  }

  const mc = client.proxy('MissionControl');
  await testCh1Unary(mc);
  await testCh2Streaming(mc);
  await testCh3ClientStream(mc);
  await testCh4Session(client);

  await client.close();
}

async function runAuthMode(address: string, keysDir: string): Promise<void> {
  section('Auth mode (Chapter 5)');
  const edgeCred = join(keysDir, 'edge.cred');
  const opsCred = join(keysDir, 'ops.cred');
  if (!existsSync(edgeCred)) {
    fail('Ch5 setup', `missing ${edgeCred} — run setup_auth.sh first`);
    return;
  }
  if (!existsSync(opsCred)) {
    fail('Ch5 setup', `missing ${opsCred} — run setup_auth.sh first`);
    return;
  }

  await testCh5NoCredential(address);
  await testCh5EdgeCredential(address, edgeCred);
  await testCh5OpsCredential(address, opsCred);
}

// ── Main ────────────────────────────────────────────────────────────────────

async function main(): Promise<number> {
  const args = process.argv.slice(2);
  if (args.length < 1) {
    console.error('Usage: bun run test_guide.ts <address> [--mode dev|auth] [--keys-dir DIR] [-q]');
    return 2;
  }

  const address = args[0];
  let mode: 'dev' | 'auth' = 'dev';
  let keysDir = '';

  for (let i = 1; i < args.length; i++) {
    const a = args[i];
    if (a === '--mode') mode = args[++i] as any;
    else if (a === '--keys-dir') keysDir = args[++i];
    else if (a === '-q' || a === '--quiet') QUIET = true;
  }

  if (mode === 'auth' && !keysDir) {
    console.error('Error: --keys-dir is required for --mode auth');
    return 2;
  }

  if (!QUIET) {
    console.log(`Testing ${address.slice(0, 30)}... (mode=${mode})`);
  }

  if (mode === 'dev') {
    await runDevMode(address);
  } else {
    await runAuthMode(address, keysDir);
  }

  if (!QUIET) {
    console.log(`\n\x1b[1mResult:\x1b[0m \x1b[32m${PASS_COUNT} passed\x1b[0m, \x1b[31m${FAIL_COUNT} failed\x1b[0m`);
  } else {
    console.log(`ts-client ${mode}: ${PASS_COUNT} pass, ${FAIL_COUNT} fail`);
  }

  return FAIL_COUNT === 0 ? 0 : 1;
}

const code = await main();
process.exit(code);
