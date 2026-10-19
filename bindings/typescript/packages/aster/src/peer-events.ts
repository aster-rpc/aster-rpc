/**
 * Which peers are connected to an `AsterServer`, and when that changes
 * (`AsterServer.peerEvents()`; semantics in Rust `aster::rpc::peers`).
 *
 * Events are per peer, not per connection: `connected` on a peer's first
 * admitted `aster/1` connection (past the node's gates, so a peer enrollment
 * or a drain refuses never appears), `disconnected` when its last one ends.
 * A consumer that falls behind the bounded queue (1024 events) gets one
 * `lagged`; its recovery is to call `peerEvents()` again and rebuild from the
 * new snapshot. Connections are never held up by a slow consumer.
 */

import type {
  PeerEventReceiver as NativePeerEventReceiver,
  PeerEventWire,
  PeerIdentity,
  PeerSnapshot,
} from '@aster-rpc/transport';

export type { PeerIdentity, PeerSnapshot };

/** Why a peer's last connection ended. */
export type DisconnectReason = 'closed' | 'timed_out' | 'revoked' | 'drained' | 'error';

/** A peer's first admitted connection opened. */
export interface PeerConnected {
  kind: 'connected';
  peer: string;
  alpn: string;
  identity: PeerIdentity;
}

/** A peer's last connection ended; `reason` is that connection's. */
export interface PeerDisconnected {
  kind: 'disconnected';
  peer: string;
  reason: DisconnectReason;
}

/**
 * The stream fell behind its bounded queue and `missed` events were dropped.
 * The consumer's state is stale: call `peerEvents()` again.
 */
export interface PeerLagged {
  kind: 'lagged';
  missed: number;
}

export type PeerEvent = PeerConnected | PeerDisconnected | PeerLagged;

function toEvent(wire: PeerEventWire): PeerEvent {
  switch (wire.kind) {
    case 'connected':
      return { kind: 'connected', peer: wire.peer!, alpn: wire.alpn!, identity: wire.identity! };
    case 'disconnected':
      return { kind: 'disconnected', peer: wire.peer!, reason: wire.reason! };
    case 'lagged':
      return { kind: 'lagged', missed: wire.missed! };
  }
}

/**
 * Async iterable of {@link PeerEvent} from the moment `peerEvents()` returned
 * it. Ends when closed or the server ends. Cancelling a pending `next()`
 * consumes nothing.
 */
export class PeerEvents implements AsyncIterableIterator<PeerEvent> {
  constructor(private readonly native: NativePeerEventReceiver) {}

  /** The next event, or `null` when closed or the server ended. */
  async recv(): Promise<PeerEvent | null> {
    const wire = await this.native.next();
    return wire ? toEvent(wire) : null;
  }

  async next(): Promise<IteratorResult<PeerEvent, undefined>> {
    const event = await this.recv();
    return event ? { value: event, done: false } : { value: undefined, done: true };
  }

  async return(): Promise<IteratorResult<PeerEvent, undefined>> {
    this.close();
    return { value: undefined, done: true };
  }

  [Symbol.asyncIterator](): this {
    return this;
  }

  /** Stop the stream; a pending and every later `next()` ends it. */
  close(): void {
    this.native.close();
  }
}

/** The peers connected now and the stream of changes after them. */
export interface PeerEventsResult {
  snapshot: PeerSnapshot[];
  events: PeerEvents;
}

/** @internal */
export function openPeerEvents(native: NativePeerEventReceiver): PeerEventsResult {
  return { snapshot: native.takeSnapshot(), events: new PeerEvents(native) };
}
