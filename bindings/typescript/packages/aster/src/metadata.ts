/**
 * Metadata — extensible semantic documentation for services, methods, and fields.
 *
 * Provides descriptions that AI agents (via MCP) and humans can use to
 * understand what services do and how to populate request fields.
 *
 * Metadata is NON-CANONICAL — it does NOT affect contract identity (BLAKE3 hash)
 * and does NOT appear in the wire protocol.
 */

import { WireType } from './decorators.js';

@WireType('_aster/Metadata')
export class Metadata {
  description = '';

  constructor(init?: Partial<Metadata>) {
    if (init) Object.assign(this, init);
  }
}
