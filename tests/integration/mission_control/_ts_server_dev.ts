#!/usr/bin/env bun
/**
 * TS launcher for the Mission Control example server in DEV mode.
 *
 * Imports the actual example services and starts an AsterServer in
 * open-gate mode. Used by run_matrix.sh.
 *
 * MUST be run from `bindings/typescript/` so that '@aster-rpc/aster'
 * resolves via the workspace package.
 */

import { AsterServer } from '@aster-rpc/aster';
import {
  MissionControl,
  AgentSession,
} from '../../../examples/typescript/missionControl/services.ts';

const server = new AsterServer({
  services: [new MissionControl(), new AgentSession()],
});
await server.start();
console.log(server.address);
await server.serve();
