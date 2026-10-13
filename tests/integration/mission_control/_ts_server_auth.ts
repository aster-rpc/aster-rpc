#!/usr/bin/env bun
/**
 * TS launcher for the Mission Control example server in AUTH mode.
 *
 * Imports the auth-enabled services (services-auth.ts) and starts an
 * AsterServer whose Enrollment comes entirely from the environment
 * (`AsterConfig.fromEnv()`): `auth.env` from setup_auth.sh plus the server's
 * key, credential and state path.
 *
 * Run from `bindings/typescript/`:
 *   set -a; . <work_dir>/auth.env; set +a
 *   ASTER_SECRET_KEY_FILE=<work_dir>/server.key \
 *   ASTER_ENROLLMENT_CREDENTIAL=<work_dir>/server.cred \
 *   ASTER_ENROLLMENT_STATE_PATH=<work_dir>/server-enrollment.json \
 *     bun run ../../tests/integration/mission_control/_ts_server_auth.ts
 */

import { AsterConfig, AsterServer, isEnrollmentEnabled } from '@aster-rpc/aster';
import {
  MissionControl,
  AgentSession,
} from '../../../examples/typescript/missionControl/services-auth.ts';

const config = AsterConfig.fromEnv();
if (!isEnrollmentEnabled(config)) {
  console.error('auth mode needs enrollment configured (source auth.env from setup_auth.sh)');
  process.exit(1);
}

const server = new AsterServer({
  services: [new MissionControl(), new AgentSession()],
  config,
});
await server.start();
console.log(server.address);
await server.serve();
