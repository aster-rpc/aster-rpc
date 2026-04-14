/**
 * Mission Control services.
 *
 * Two services with different lifetimes:
 *
 *   MissionControl (shared)   — fleet-wide: status, logs, metrics
 *   AgentSession   (session)  — per-agent: register, heartbeat, commands
 *
 * Services are defined without requires= so they work out of the box in
 * dev mode (open gate, no credentials). See services-auth.ts for the
 * variant with role-based access control (Chapter 5).
 */

import { execFile } from 'node:child_process';
import { promisify } from 'node:util';
import {
  Service,
  Rpc,
  ServerStream,
  ClientStream,
  BidiStream,
} from '@aster-rpc/aster';

const exec = promisify(execFile);

import {
  StatusRequest,
  StatusResponse,
  LogEntry,
  SubmitLogResult,
  TailRequest,
  MetricPoint,
  IngestResult,
  Heartbeat,
  Assignment,
  Command,
  CommandResult,
} from './types.js';

const LOG_LEVEL_RANK: Record<string, number> = {
  debug: 0, info: 1, warn: 2, error: 3,
};

@Service({ name: "MissionControl", version: 1 })
export class MissionControl {
  private logQueue: LogEntry[] = [];
  private logWaiters: ((entry: LogEntry) => void)[] = [];
  private metrics: MetricPoint[] = [];

  // -- Chapter 1: status ------------------------------------------------------

  @Rpc()
  async getStatus(req: StatusRequest): Promise<StatusResponse> {
    return new StatusResponse({
      agent_id: req.agent_id,
      status: "running",
      uptime_secs: 3600,
    });
  }

  // -- Chapter 2: logging -----------------------------------------------------

  @Rpc()
  async submitLog(entry: LogEntry): Promise<SubmitLogResult> {
    // Wake any waiting tailLogs streams
    if (this.logWaiters.length > 0) {
      const waiter = this.logWaiters.shift()!;
      waiter(entry);
    } else {
      this.logQueue.push(entry);
    }
    return new SubmitLogResult({ accepted: true });
  }

  @ServerStream()
  async *tailLogs(req: TailRequest): AsyncGenerator<LogEntry> {
    const minRank = LOG_LEVEL_RANK[req.level?.toLowerCase() ?? "info"] ?? 0;
    while (true) {
      const entry = await this.nextLog();
      if (req.agent_id && entry.agent_id !== req.agent_id) continue;
      if ((LOG_LEVEL_RANK[entry.level?.toLowerCase() ?? "info"] ?? 0) < minRank) continue;
      yield entry;
    }
  }

  private nextLog(): Promise<LogEntry> {
    const queued = this.logQueue.shift();
    if (queued) return Promise.resolve(queued);
    return new Promise<LogEntry>((resolve) => {
      this.logWaiters.push(resolve);
    });
  }

  // -- Chapter 3: metrics -----------------------------------------------------

  @ClientStream()
  async ingestMetrics(stream: AsyncIterable<MetricPoint>): Promise<IngestResult> {
    let accepted = 0;
    for await (const point of stream) {
      this.metrics.push(point);
      accepted++;
    }
    return new IngestResult({ accepted });
  }
}

@Service({ name: "AgentSession", version: 1, scoped: "session" })
export class AgentSession {
  private _agentId = "";
  private _capabilities: string[] = [];

  @Rpc()
  async register(hb: Heartbeat): Promise<Assignment> {
    this._agentId = hb.agent_id;
    this._capabilities = [...(hb.capabilities ?? [])];
    if (this._capabilities.includes("gpu")) {
      return new Assignment({ task_id: "train-42", command: "python train.py" });
    }
    return new Assignment({ task_id: "idle", command: "sleep 60" });
  }

  @Rpc()
  async heartbeat(hb: Heartbeat): Promise<Assignment> {
    this._capabilities = [...(hb.capabilities ?? [])];
    return new Assignment({ task_id: "continue", command: "" });
  }

  @BidiStream()
  async *runCommand(commands: AsyncIterable<Command>): AsyncGenerator<CommandResult> {
    for await (const cmd of commands) {
      try {
        const { stdout, stderr } = await exec("sh", ["-c", cmd.command]);
        yield new CommandResult({ stdout, stderr, exit_code: 0 });
      } catch (e: any) {
        yield new CommandResult({
          stdout: e.stdout ?? "",
          stderr: e.stderr ?? e.message,
          exit_code: e.code ?? 1,
        });
      }
    }
  }
}
