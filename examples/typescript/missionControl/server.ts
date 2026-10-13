#!/usr/bin/env bun
/**
 * Mission Control server (producer).
 *
 * Chapters 1-4: dev mode (open node, ephemeral keys).
 *
 *     bun run server.ts
 *
 * Chapter 5: only enrolled nodes connect, and each method's `requires` is
 * checked against the roles in the caller's credential. Everything comes
 * from the configuration (`ASTER_ROOT_PUBKEY`, `ASTER_ENROLLMENT_*`, ... or
 * an `aster.toml` passed with `--config`); see the README for the `aster`
 * CLI steps that produce the files.
 *
 *     ASTER_ROOT_PUBKEY=... ASTER_ENROLLMENT_AUDIENCE=mission-control \
 *     ASTER_ENROLLMENT_FLEET_NAMESPACE=... ASTER_SECRET_KEY_FILE=server.key \
 *     ASTER_ENROLLMENT_CREDENTIAL=server.cred \
 *     ASTER_ENROLLMENT_STATE_PATH=./mc-enrollment.json \
 *       bun run server.ts --auth
 */

import { AsterConfig, AsterServer, isEnrollmentEnabled } from '@aster-rpc/aster';

const auth = process.argv.includes("--auth");
// `--config aster.toml` reads the file (the environment still overrides it).
const configFlag = process.argv.indexOf("--config");
const config = configFlag > 0
  ? AsterConfig.fromFile(process.argv[configFlag + 1])
  : AsterConfig.fromEnv();

let services: object[];
if (auth) {
  if (!isEnrollmentEnabled(config)) {
    console.error(
      "--auth needs enrollment configured: ASTER_ROOT_PUBKEY, ASTER_ENROLLMENT_AUDIENCE, " +
      "ASTER_ENROLLMENT_FLEET_NAMESPACE, ASTER_ENROLLMENT_CREDENTIAL and " +
      "ASTER_ENROLLMENT_STATE_PATH (see the README)",
    );
    process.exit(1);
  }
  const { MissionControl, AgentSession } = await import('./services-auth.js');
  services = [new MissionControl(), new AgentSession()];
} else {
  const { MissionControl, AgentSession } = await import('./services.js');
  services = [new MissionControl(), new AgentSession()];
}

const server = new AsterServer({ services, config });

await server.start();
console.log(server.address);
await server.serve();
