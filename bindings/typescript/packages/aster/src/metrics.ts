/**
 * Connection and admission metrics.
 *
 * Lightweight in-memory counters for monitoring connection and
 * admission activity. Complements the RPC-level MetricsInterceptor.
 */

/** Connection metrics counters. */
export class ConnectionMetrics {
  connectionsAccepted = 0;
  connectionsRejected = 0;
  connectionsClosed = 0;
  connectionsActive = 0;

  onAccept(): void {
    this.connectionsAccepted++;
    this.connectionsActive++;
  }

  onReject(): void {
    this.connectionsRejected++;
  }

  onClose(): void {
    this.connectionsClosed++;
    this.connectionsActive = Math.max(0, this.connectionsActive - 1);
  }

  snapshot(): Record<string, number> {
    return {
      connections_accepted: this.connectionsAccepted,
      connections_rejected: this.connectionsRejected,
      connections_closed: this.connectionsClosed,
      connections_active: this.connectionsActive,
    };
  }

  reset(): void {
    this.connectionsAccepted = 0;
    this.connectionsRejected = 0;
    this.connectionsClosed = 0;
    this.connectionsActive = 0;
  }
}

/** Admission metrics counters. */
export class AdmissionMetrics {
  admissionsAttempted = 0;
  admissionsSucceeded = 0;
  admissionsRejected = 0;
  admissionsErrored = 0;

  onAttempt(): void {
    this.admissionsAttempted++;
  }

  onSuccess(): void {
    this.admissionsSucceeded++;
  }

  onReject(): void {
    this.admissionsRejected++;
  }

  onError(): void {
    this.admissionsErrored++;
  }

  snapshot(): Record<string, number> {
    return {
      admissions_attempted: this.admissionsAttempted,
      admissions_succeeded: this.admissionsSucceeded,
      admissions_rejected: this.admissionsRejected,
      admissions_errored: this.admissionsErrored,
    };
  }

  reset(): void {
    this.admissionsAttempted = 0;
    this.admissionsSucceeded = 0;
    this.admissionsRejected = 0;
    this.admissionsErrored = 0;
  }
}
