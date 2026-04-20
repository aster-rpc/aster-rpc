#!/usr/bin/env bun
/**
 * TS launcher for the Mission Control example server in DEV mode.
 *
 * Imports the actual example services and starts an AsterServer in
 * open-gate mode. Used by run_matrix.sh.
 *
 * The generated metadata is imported explicitly and passed to AsterServer
 * to avoid runtime dynamic import issues.
 */

import { AsterServer } from '@aster-rpc/aster';
import {
  MissionControl,
  AgentSession,
} from '../../../examples/typescript/missionControl/services.ts';
import { SERVICES, WIRE_TYPES, BUILD_ALL_TYPES } from '../../../examples/typescript/missionControl/aster-rpc.generated.ts';

const server = new AsterServer({
  services: [new MissionControl(), new AgentSession()],
  generated: {
    SERVICES,
    WIRE_TYPES,
    buildAllTypes: BUILD_ALL_TYPES,
  } as any,
});
await server.start();
console.log(server.address);
await server.serve();
