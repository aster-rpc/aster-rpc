/**
 * Health and readiness HTTP endpoints.
 *
 * Provides /healthz, /readyz, /metrics, and /metrics/prometheus endpoints
 * for Kubernetes probes and monitoring. Disabled by default (port 0).
 *
 * Default bind: 127.0.0.1 (localhost only). Set 0.0.0.0 for k8s pods.
 */

import { createServer, type Server, type IncomingMessage, type ServerResponse } from 'node:http';

export interface HealthState {
  isHealthy: () => boolean;
  isReady: () => boolean;
  metrics: () => HealthMetrics;
}

export interface HealthMetrics {
  health: { status: string; uptimeS: number };
  ready: { status: string; services: number };
  rpc: { started: number; succeeded: number; failed: number; inFlight: number };
  connections: { active: number; total: number };
}

/**
 * Lightweight HTTP health server.
 *
 * @example
 * ```ts
 * const health = new HealthServer({ port: 8080 });
 * health.setState({ isHealthy: () => true, isReady: () => true, metrics: () => ({...}) });
 * await health.start();
 * ```
 */
export class HealthServer {
  private server: Server | null = null;
  private state: HealthState | null = null;
  private startTime = Date.now();
  private port: number;
  private host: string;

  constructor(opts: { port?: number; host?: string } = {}) {
    this.port = opts.port ?? 0;
    this.host = opts.host ?? '127.0.0.1';
  }

  setState(state: HealthState): void {
    this.state = state;
  }

  async start(): Promise<void> {
    if (this.port === 0) return; // disabled

    this.server = createServer((req, res) => this.handle(req, res));
    return new Promise((resolve) => {
      this.server!.listen(this.port, this.host, () => resolve());
    });
  }

  async stop(): Promise<void> {
    if (this.server) {
      return new Promise((resolve) => this.server!.close(() => resolve()));
    }
  }

  private handle(req: IncomingMessage, res: ServerResponse): void {
    const url = req.url ?? '/';

    if (url === '/healthz') {
      const ok = this.state?.isHealthy() ?? true;
      const uptimeS = (Date.now() - this.startTime) / 1000;
      res.writeHead(ok ? 200 : 503, { 'Content-Type': 'application/json' });
      res.end(JSON.stringify({ status: ok ? 'ok' : 'unhealthy', uptime_s: uptimeS }));
    } else if (url === '/readyz') {
      const ok = this.state?.isReady() ?? false;
      res.writeHead(ok ? 200 : 503, { 'Content-Type': 'application/json' });
      res.end(JSON.stringify({ status: ok ? 'ready' : 'not_ready' }));
    } else if (url === '/metrics') {
      const m = this.state?.metrics();
      res.writeHead(200, { 'Content-Type': 'application/json' });
      res.end(JSON.stringify(m ?? {}));
    } else if (url === '/metrics/prometheus') {
      const m = this.state?.metrics();
      res.writeHead(200, { 'Content-Type': 'text/plain' });
      if (m) {
        const lines = [
          `# HELP aster_rpc_started_total Total RPC calls started`,
          `# TYPE aster_rpc_started_total counter`,
          `aster_rpc_started_total ${m.rpc.started}`,
          `# HELP aster_rpc_succeeded_total Total successful RPCs`,
          `# TYPE aster_rpc_succeeded_total counter`,
          `aster_rpc_succeeded_total ${m.rpc.succeeded}`,
          `# HELP aster_rpc_failed_total Total failed RPCs`,
          `# TYPE aster_rpc_failed_total counter`,
          `aster_rpc_failed_total ${m.rpc.failed}`,
          `# HELP aster_rpc_in_flight Current in-flight RPCs`,
          `# TYPE aster_rpc_in_flight gauge`,
          `aster_rpc_in_flight ${m.rpc.inFlight}`,
          `# HELP aster_connections_active Active connections`,
          `# TYPE aster_connections_active gauge`,
          `aster_connections_active ${m.connections.active}`,
        ];
        res.end(lines.join('\n') + '\n');
      } else {
        res.end('');
      }
    } else {
      res.writeHead(404);
      res.end('not found');
    }
  }
}
