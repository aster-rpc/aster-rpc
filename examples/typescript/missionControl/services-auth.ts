/**
 * Mission Control services with role-based access control (Chapter 5).
 *
 * Same services as services.ts but with requires= on each method.
 * Used by: bun run server.ts --auth
 */

import {
  Service,
  Rpc,
  ServerStream,
  ClientStream,
  BidiStream,
  anyOf,
} from '@aster-rpc/aster';

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

import { Role } from './roles.js';

const LOG_LEVEL_RANK: Record<string, number> = {
  debug: 0, info: 1, warn: 2, error: 3,
};

@Service({ name: "MissionControl", version: 1 })
export class MissionControl {
  private logQueue: LogEntry[] = [];
  private logWaiters: ((entry: LogEntry) => void)[] = [];
  private metrics: MetricPoint[] = [];

  @Rpc({ requires: Role.STATUS })
  async getStatus(req: StatusRequest): Promise<StatusResponse> {
    return new StatusResponse({
      agent_id: req.agent_id,
      status: "running",
      uptime_secs: 3600,
    });
  }

  @Rpc()
  async submitLog(entry: LogEntry): Promise<SubmitLogResult> {
    if (this.logWaiters.length > 0) {
      const waiter = this.logWaiters.shift()!;
      waiter(entry);
    } else {
      this.logQueue.push(entry);
    }
    return new SubmitLogResult({ accepted: true });
  }

  @ServerStream({ requires: anyOf(Role.LOGS, Role.ADMIN) })
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

  @ClientStream({ requires: Role.INGEST })
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

  @Rpc({ requires: Role.INGEST })
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

  @BidiStream({ requires: Role.ADMIN })
  async *runCommand(commands: AsyncIterable<Command>): AsyncGenerator<CommandResult> {
    for await (const cmd of commands) {
      const proc = Bun.spawn(["sh", "-c", cmd.command], {
        stdout: "pipe",
        stderr: "pipe",
      });
      const stdout = await new Response(proc.stdout).text();
      const stderr = await new Response(proc.stderr).text();
      const exit_code = await proc.exited;
      yield new CommandResult({ stdout, stderr, exit_code });
    }
  }
}
