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
 * Against `server.ts --auth` (Chapter 5), configure the operator's own key
 * and credential (roles `ops.status,ops.logs`) the same way as the agent:
 * ASTER_SECRET_KEY_FILE=operator.key ASTER_ENROLLMENT_CREDENTIAL=operator.cred
 * plus ASTER_ROOT_PUBKEY / ASTER_ENROLLMENT_AUDIENCE /
 * ASTER_ENROLLMENT_FLEET_NAMESPACE.
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
const mc = await client.proxy("MissionControl");

console.log("Tailing logs (Ctrl+C to stop)...");

for await (const entry of mc.tailLogs.stream({ level: "info" })) {
  const e = entry as Record<string, unknown>;
  const level = String(e.level ?? "?").padStart(5);
  const agent = e.agent_id ?? "";
  const msg = e.message ?? "";
  console.log(`  [${level}] ${agent}: ${msg}`);
}

await client.close();
