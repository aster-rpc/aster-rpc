#!/usr/bin/env bun
/**
 * Example operator that tails logs from Mission Control.
 *
 * Demonstrates server streaming (Chapter 2):
 *   - tailLogs streams log entries as they arrive
 *
 * Usage:
 *     bun run operator.ts <server-address>
 *
 * Press Ctrl+C to stop tailing.
 */

import { AsterClientWrapper } from '@aster-rpc/aster';

const address = process.argv[2];
if (!address) {
  console.error("Usage: bun run operator.ts <server-address>");
  process.exit(1);
}

const client = new AsterClientWrapper({ address });
await client.connect();
const mc = client.proxy("MissionControl");

console.log("Tailing logs (Ctrl+C to stop)...");

for await (const entry of mc.tailLogs.stream({ level: "info" })) {
  const e = entry as Record<string, unknown>;
  const level = String(e.level ?? "?").padStart(5);
  const agent = e.agentId ?? e.agent_id ?? "";
  const msg = e.message ?? "";
  console.log(`  [${level}] ${agent}: ${msg}`);
}

await client.close();
