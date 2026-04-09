#!/usr/bin/env bun
/**
 * Mission Control server (producer).
 *
 * Chapters 1-4: dev mode (open gate, ephemeral keys).
 *
 *     bun run server.ts
 *
 * Chapter 5: production mode with auth.
 *
 *     ASTER_ROOT_PUBKEY_FILE=~/.aster/root.pub bun run server.ts --auth
 */

import { AsterServer } from '@aster-rpc/aster';

const auth = process.argv.includes("--auth");

let services: object[];
if (auth) {
  const { MissionControl, AgentSession } = await import('./services-auth.js');
  services = [new MissionControl(), new AgentSession()];
} else {
  const { MissionControl, AgentSession } = await import('./services.js');
  services = [new MissionControl(), new AgentSession()];
}

const server = new AsterServer({
  services,
  allowAllConsumers: !auth,
});

await server.start();
console.log(server.address);
await server.serve();
