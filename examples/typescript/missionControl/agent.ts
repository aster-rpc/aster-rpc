#!/usr/bin/env bun
/**
 * Example agent that connects to Mission Control.
 *
 * Demonstrates proxy client usage (Chapters 1-3):
 *   - getStatus (unary)
 *   - submitLog (unary)
 *   - ingestMetrics (client streaming)
 *
 * Usage:
 *     bun run agent.ts <server-address>
 */

import { AsterClientWrapper } from '@aster-rpc/aster';

const address = process.argv[2];
if (!address) {
  console.error("Usage: bun run agent.ts <server-address>");
  process.exit(1);
}

const client = new AsterClientWrapper({ address });
await client.connect();
const mc = client.proxy("MissionControl");

// Chapter 1: check in
const status = await mc.getStatus({ agentId: "ts-agent-1" });
console.log("Status:", status);

// Chapter 2: push a log entry
await mc.submitLog({
  timestamp: Date.now() / 1000,
  level: "info",
  message: "agent started",
  agentId: "ts-agent-1",
});
console.log("Log submitted");

// Chapter 3: stream 1000 metrics
async function* metrics() {
  for (let i = 0; i < 1000; i++) {
    yield {
      name: "cpu.usage",
      value: Math.random() * 100,
      timestamp: Date.now() / 1000,
    };
  }
}

const result = await mc.ingestMetrics(metrics());
console.log(`Metrics accepted: ${(result as any).accepted}`);

await client.close();
