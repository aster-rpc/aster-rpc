/**
 * Registry gossip — broadcasts and listens for registry events.
 *
 * Mirrors bindings/python/aster/registry/gossip.py.
 */

import type { GossipEvent } from './models.js';
import { GossipEventType as ET } from './models.js';

/** Gossip topic handle interface (matches NAPI GossipTopicHandle). */
interface GossipTopic {
  broadcast(data: Uint8Array): Promise<void>;
  recv(): Promise<Uint8Array>;
}

/**
 * Registry gossip — broadcasts events and listens for updates.
 */
export class RegistryGossip {
  private topic: GossipTopic;
  private encoder = new TextEncoder();
  private decoder = new TextDecoder();

  constructor(topic: GossipTopic) {
    this.topic = topic;
  }

  /** Broadcast a contract publication event. */
  async broadcastContractPublished(contractId: string, service: string, version: number): Promise<void> {
    await this.broadcast({
      type: ET.CONTRACT_PUBLISHED,
      contractId,
      service,
      version,
      timestampMs: Date.now(),
    });
  }

  /** Broadcast a channel update event. */
  async broadcastChannelUpdated(service: string, channel: string, contractId: string): Promise<void> {
    await this.broadcast({
      type: ET.CHANNEL_UPDATED,
      service,
      channel,
      contractId,
      timestampMs: Date.now(),
    });
  }

  /** Broadcast an endpoint lease upsert event. */
  async broadcastEndpointLeaseUpserted(
    endpointId: string, service: string, leaseSeq: number, contractId: string,
  ): Promise<void> {
    await this.broadcast({
      type: ET.ENDPOINT_LEASE_UPSERTED,
      endpointId,
      service,
      version: leaseSeq,
      contractId,
      timestampMs: Date.now(),
    });
  }

  /** Broadcast an endpoint down event. */
  async broadcastEndpointDown(endpointId: string, service: string): Promise<void> {
    await this.broadcast({
      type: ET.ENDPOINT_DOWN,
      endpointId,
      service,
      timestampMs: Date.now(),
    });
  }

  /** Broadcast an ACL change event. */
  async broadcastAclChanged(keyPrefix: string): Promise<void> {
    await this.broadcast({
      type: ET.ACL_CHANGED,
      keyPrefix,
      timestampMs: Date.now(),
    });
  }

  /**
   * Listen for gossip events. Yields events as they arrive.
   * Silently skips malformed messages.
   */
  async *listen(): AsyncGenerator<GossipEvent> {
    while (true) {
      try {
        const data = await this.topic.recv();
        const text = this.decoder.decode(data);
        const event: GossipEvent = JSON.parse(text);
        yield event;
      } catch {
        // Skip undecipherable messages
        continue;
      }
    }
  }

  // -- Helper --

  private async broadcast(event: GossipEvent): Promise<void> {
    const data = this.encoder.encode(JSON.stringify(event));
    await this.topic.broadcast(data);
  }
}
