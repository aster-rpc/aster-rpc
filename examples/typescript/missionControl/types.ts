/**
 * Wire types for the Mission Control example.
 *
 * Each type has a stable wire identity via @WireType so any Aster binding
 * that declares the same contract can interoperate. Tags and field names
 * match the Python example byte-for-byte, making Python+TS a single shared
 * contract for this service.
 *
 * Field names are snake_case because the Python example declared them that
 * way and this TS example chose to implement the same contract. The codec
 * does no name normalization — producer wins, consumers mirror. If a
 * language wants camelCase fields for the same service, it is declaring a
 * different contract and will receive a different contract_id.
 */

import { WireType } from '@aster-rpc/aster';

// -- Chapter 1: Status --------------------------------------------------------

@WireType("mission/StatusRequest")
export class StatusRequest {
  agent_id = "";
  constructor(init?: Partial<StatusRequest>) { if (init) Object.assign(this, init); }
}

@WireType("mission/StatusResponse")
export class StatusResponse {
  agent_id = "";
  status = "idle";
  uptime_secs = 0;
  constructor(init?: Partial<StatusResponse>) { if (init) Object.assign(this, init); }
}

// -- Chapter 2: Logging -------------------------------------------------------

@WireType("mission/LogEntry")
export class LogEntry {
  timestamp = 0;
  level = "info";
  message = "";
  agent_id = "";
  constructor(init?: Partial<LogEntry>) { if (init) Object.assign(this, init); }
}

@WireType("mission/SubmitLogResult")
export class SubmitLogResult {
  accepted = true;
  constructor(init?: Partial<SubmitLogResult>) { if (init) Object.assign(this, init); }
}

@WireType("mission/TailRequest")
export class TailRequest {
  agent_id = "";
  level = "info";
  constructor(init?: Partial<TailRequest>) { if (init) Object.assign(this, init); }
}

// -- Chapter 3: Metrics -------------------------------------------------------

@WireType("mission/MetricPoint")
export class MetricPoint {
  name = "";
  value = 0;
  timestamp = 0;
  tags: Record<string, string> = {};
  constructor(init?: Partial<MetricPoint>) { if (init) Object.assign(this, init); }
}

@WireType("mission/IngestResult")
export class IngestResult {
  accepted = 0;
  dropped = 0;
  constructor(init?: Partial<IngestResult>) { if (init) Object.assign(this, init); }
}

// -- Chapter 4: Sessions & Commands -------------------------------------------

@WireType("mission/Heartbeat")
export class Heartbeat {
  agent_id = "";
  capabilities: string[] = [];
  load_avg = 0;
  constructor(init?: Partial<Heartbeat>) { if (init) Object.assign(this, init); }
}

@WireType("mission/Assignment")
export class Assignment {
  task_id = "";
  command = "";
  constructor(init?: Partial<Assignment>) { if (init) Object.assign(this, init); }
}

@WireType("mission/Command")
export class Command {
  command = "";
  constructor(init?: Partial<Command>) { if (init) Object.assign(this, init); }
}

@WireType("mission/CommandResult")
export class CommandResult {
  stdout = "";
  stderr = "";
  exit_code = -1;
  constructor(init?: Partial<CommandResult>) { if (init) Object.assign(this, init); }
}
