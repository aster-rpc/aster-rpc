#!/usr/bin/env bun
/**
 * TS launcher for the Mission Control example server in AUTH mode.
 *
 * Imports the auth-enabled services (services-auth.ts) and starts an
 * AsterServer in trusted mode using ASTER_ROOT_PUBKEY_FILE.
 *
 * Run from `bindings/typescript/`:
 *   ASTER_ROOT_PUBKEY_FILE=/path/to/root.pub \
 *     bun run ../../tests/integration/mission_control/_ts_server_auth.ts
 */

import { AsterServer } from '@aster-rpc/aster';
import {
  MissionControl,
  AgentSession,
} from '../../../examples/typescript/missionControl/services-auth.ts';


const rootPubkeyFile = process.env.ASTER_ROOT_PUBKEY_FILE;
if (!rootPubkeyFile) {
  console.error('ASTER_ROOT_PUBKEY_FILE must be set for auth mode');
  process.exit(1);
}

const server = new AsterServer({
  services: [new MissionControl(), new AgentSession()],
  config: { rootPubkeyFile },
  allowAllConsumers: false,
});
await server.start();
console.log(server.address);
await server.serve();
