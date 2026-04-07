/**
 * Health and readiness HTTP endpoints.
 *
 * Provides /healthz, /readyz, /metrics, and /metrics/prometheus endpoints
 * for Kubernetes probes and monitoring. Disabled by default (port 0).
 *
 * Default bind: 127.0.0.1 (localhost only). Set 0.0.0.0 for k8s pods.
 */

import { createServer, type Server, type IncomingMessage, type ServerResponse } from 'node:http';

// ── Singleton metrics stores ──────────────────────────────────────────────────

/** Connection-level metrics (singleton). */
export class ConnectionMetrics {
  opened = 0;
  closed = 0;
  streamsOpened = 0;
  streamsClosed = 0;

  connectionOpened(): void { this.opened++; }
  connectionClosed(): void { this.closed++; }
  streamOpened(): void { this.streamsOpened++; }
  streamClosed(): void { this.streamsClosed++; }

  snapshot(): Record<string, number> {
    return {
      connections_opened: this.opened,
      connections_closed: this.closed,
      streams_active: this.streamsOpened - this.streamsClosed,
      streams_total: this.streamsOpened,
    };
  }
}

/** Admission-level metrics (singleton). */
export class AdmissionMetrics {
  consumerAdmits = 0;
  consumerDenies = 0;
  consumerErrors = 0;
  producerAdmits = 0;
  producerDenies = 0;
  producerErrors = 0;
  lastAdmissionMs = 0;
  totalAdmissionMs = 0;

  recordConsumerAdmit(durationMs = 0): void {
    this.consumerAdmits++;
    this.lastAdmissionMs = durationMs;
    this.totalAdmissionMs += durationMs;
  }
  recordConsumerDeny(): void { this.consumerDenies++; }
  recordConsumerError(): void { this.consumerErrors++; }
  recordProducerAdmit(durationMs = 0): void {
    this.producerAdmits++;
    this.lastAdmissionMs = durationMs;
    this.totalAdmissionMs += durationMs;
  }
  recordProducerDeny(): void { this.producerDenies++; }
  recordProducerError(): void { this.producerErrors++; }

  snapshot(): Record<string, number> {
    return {
      consumer_admits: this.consumerAdmits,
      consumer_denies: this.consumerDenies,
      consumer_errors: this.consumerErrors,
      producer_admits: this.producerAdmits,
      producer_denies: this.producerDenies,
      producer_errors: this.producerErrors,
      last_admission_ms: this.lastAdmissionMs,
    };
  }
}

const _connectionMetrics = new ConnectionMetrics();
const _admissionMetrics = new AdmissionMetrics();

/** Get the singleton connection metrics instance. */
export function getConnectionMetrics(): ConnectionMetrics {
  return _connectionMetrics;
}

/** Get the singleton admission metrics instance. */
export function getAdmissionMetrics(): AdmissionMetrics {
  return _admissionMetrics;
}

/** Reset all singleton metrics. */
export function resetMetrics(): void {
  const cm = _connectionMetrics;
  cm.opened = 0; cm.closed = 0; cm.streamsOpened = 0; cm.streamsClosed = 0;
  const am = _admissionMetrics;
  am.consumerAdmits = 0; am.consumerDenies = 0; am.consumerErrors = 0;
  am.producerAdmits = 0; am.producerDenies = 0; am.producerErrors = 0;
  am.lastAdmissionMs = 0; am.totalAdmissionMs = 0;
}

/** Check health of a server (returns true if running). */
export function checkHealth(server: { running?: boolean }): boolean {
  return server.running !== false;
}

/** Check readiness of a server (returns true if running and not draining). */
export function checkReady(server: { running?: boolean; draining?: boolean }): boolean {
  return server.running !== false && server.draining !== true;
}

/** Return health status as a JSON-serializable object. */
export function healthStatus(server: { running?: boolean }): Record<string, unknown> {
  return {
    status: checkHealth(server) ? 'ok' : 'unhealthy',
    uptime_s: 0,
  };
}

/** Return readiness status as a JSON-serializable object. */
export function readyStatus(server: { running?: boolean; draining?: boolean }): Record<string, unknown> {
  return {
    status: checkReady(server) ? 'ready' : 'not_ready',
  };
}

/** Return a full metrics snapshot. */
export function metricsSnapshot(_server?: unknown): Record<string, unknown> {
  return {
    connections: _connectionMetrics.snapshot(),
    admission: _admissionMetrics.snapshot(),
  };
}

export interface HealthState {
  isHealthy: () => boolean;
  isReady: () => boolean;
  metrics: () => HealthMetrics;
}

export interface HealthMetrics {
  health: { status: string; uptimeS: number };
  ready: { status: string; services: number };
  rpc: {
    started: number;
    succeeded: number;
    failed: number;
    inFlight: number;
    totalDurationS: number;
    lastDurationS: number;
  };
  connections: {
    active: number;
    total: number;
    streamsActive: number;
    streamsTotal: number;
  };
  admission: {
    attempted: number;
    succeeded: number;
    rejected: number;
    errored: number;
    lastAdmissionMs: number;
  };
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
      res.writeHead(200, { 'Content-Type': 'text/plain' });
      res.end(this.renderPrometheus());
    } else {
      res.writeHead(404);
      res.end('not found');
    }
  }

  private renderPrometheus(): string {
    const m = this.state?.metrics();
    if (!m) return '';

    const uptimeS = (Date.now() - this.startTime) / 1000;
    const lines: string[] = [];

    // Uptime
    lines.push('# HELP aster_uptime_seconds Server uptime in seconds');
    lines.push('# TYPE aster_uptime_seconds gauge');
    lines.push(`aster_uptime_seconds ${uptimeS.toFixed(1)}`);

    // RPC metrics
    lines.push('# HELP aster_rpc_started_total Total RPC calls started');
    lines.push('# TYPE aster_rpc_started_total counter');
    lines.push(`aster_rpc_started_total ${m.rpc.started}`);
    lines.push('# HELP aster_rpc_succeeded_total Total successful RPCs');
    lines.push('# TYPE aster_rpc_succeeded_total counter');
    lines.push(`aster_rpc_succeeded_total ${m.rpc.succeeded}`);
    lines.push('# HELP aster_rpc_failed_total Total failed RPCs');
    lines.push('# TYPE aster_rpc_failed_total counter');
    lines.push(`aster_rpc_failed_total ${m.rpc.failed}`);
    lines.push('# HELP aster_rpc_in_flight Current in-flight RPCs');
    lines.push('# TYPE aster_rpc_in_flight gauge');
    lines.push(`aster_rpc_in_flight ${m.rpc.inFlight}`);
    lines.push('# HELP aster_rpc_duration_seconds_total Total RPC duration in seconds');
    lines.push('# TYPE aster_rpc_duration_seconds_total counter');
    lines.push(`aster_rpc_duration_seconds_total ${m.rpc.totalDurationS.toFixed(6)}`);

    // Connection metrics
    lines.push('# HELP aster_connections_active Active connections');
    lines.push('# TYPE aster_connections_active gauge');
    lines.push(`aster_connections_active ${m.connections.active}`);
    lines.push('# HELP aster_connections_total Total connections accepted');
    lines.push('# TYPE aster_connections_total counter');
    lines.push(`aster_connections_total ${m.connections.total}`);
    lines.push('# HELP aster_streams_active Active RPC streams');
    lines.push('# TYPE aster_streams_active gauge');
    lines.push(`aster_streams_active ${m.connections.streamsActive}`);
    lines.push('# HELP aster_streams_total Total streams handled');
    lines.push('# TYPE aster_streams_total counter');
    lines.push(`aster_streams_total ${m.connections.streamsTotal}`);

    // Admission metrics
    lines.push('# HELP aster_admission_attempted_total Total admission attempts');
    lines.push('# TYPE aster_admission_attempted_total counter');
    lines.push(`aster_admission_attempted_total ${m.admission.attempted}`);
    lines.push('# HELP aster_admission_succeeded_total Total successful admissions');
    lines.push('# TYPE aster_admission_succeeded_total counter');
    lines.push(`aster_admission_succeeded_total ${m.admission.succeeded}`);
    lines.push('# HELP aster_admission_rejected_total Total rejected admissions');
    lines.push('# TYPE aster_admission_rejected_total counter');
    lines.push(`aster_admission_rejected_total ${m.admission.rejected}`);

    return lines.join('\n') + '\n';
  }
}
