/**
 * Wire types for the Mission Control example.
 *
 * Each type has a stable wire identity via @WireType so Python (or any
 * other Aster binding) can interoperate. Tags match the Python example
 * exactly — that's how cross-language RPC works.
 */

import { WireType } from '@aster-rpc/aster';

// -- Chapter 1: Status --------------------------------------------------------

@WireType("mission/StatusRequest")
export class StatusRequest {
  agentId = "";
  constructor(init?: Partial<StatusRequest>) { if (init) Object.assign(this, init); }
}

@WireType("mission/StatusResponse")
export class StatusResponse {
  agentId = "";
  status = "idle";
  uptimeSecs = 0;
  constructor(init?: Partial<StatusResponse>) { if (init) Object.assign(this, init); }
}

// -- Chapter 2: Logging -------------------------------------------------------

@WireType("mission/LogEntry")
export class LogEntry {
  timestamp = 0;
  level = "info";
  message = "";
  agentId = "";
  constructor(init?: Partial<LogEntry>) { if (init) Object.assign(this, init); }
}

@WireType("mission/SubmitLogResult")
export class SubmitLogResult {
  accepted = true;
  constructor(init?: Partial<SubmitLogResult>) { if (init) Object.assign(this, init); }
}

@WireType("mission/TailRequest")
export class TailRequest {
  agentId = "";
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
  agentId = "";
  capabilities: string[] = [];
  loadAvg = 0;
  constructor(init?: Partial<Heartbeat>) { if (init) Object.assign(this, init); }
}

@WireType("mission/Assignment")
export class Assignment {
  taskId = "";
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
  exitCode = -1;
  constructor(init?: Partial<CommandResult>) { if (init) Object.assign(this, init); }
}
