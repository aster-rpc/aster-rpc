/**
 * Expose — local services over Aster (Rust `aster::expose`).
 *
 * The slice available to JavaScript today: L4 **socket** targets (a local
 * TCP service, e.g. an HTTP server on loopback) whose per-`(connection,
 * service)` authorization is a JavaScript function, a node-owned accept
 * loop, and the consumer's raw relay stream
 * (`connection.openExposeStream(serviceId, metadata)`). A JavaScript HTTP
 * *handler* and the buffered HTTP relay against it wait for the
 * bytes-level handler (R9).
 *
 * ```ts
 * const host = await Node.start({ alpns: ['aster-expose/demo'] });
 * const expose = host.expose();
 * expose.exposeSocket('web', '127.0.0.1:8080', async ({ peer }) => allowed.has(peer));
 * const server = host.serveExpose(expose, ['aster-expose/demo']);
 *
 * // elsewhere: a raw stream spliced to 127.0.0.1:8080 on the host
 * const conn = await client.connect(host.addr, 'aster-expose/demo');
 * const stream = await conn.openExposeStream('web');
 * ```
 *
 * Authorization runs over upcalls (kind `expose_authorize`): an error or
 * timeout applies the kind's failure mode (fail-closed by default).
 * @module
 */

import type { Expose as NativeExpose } from '@aster-rpc/transport';
import type { NodeId } from './node.js';
import { adapterTable, type UpcallContext, type Upcalls } from './upcalls.js';

/** Who is asking for an exposed service. @group Expose */
export interface ExposeAuthorizeRequest {
  serviceId: string;
  /** The authenticated peer. */
  peer: NodeId;
  /** Metadata the peer attached to the connection. */
  metadata: Uint8Array;
}

/** Admit (`true`) or refuse a peer for one service. @group Expose */
export type ExposeAuthorizer = (request: ExposeAuthorizeRequest, ctx: UpcallContext) => Promise<boolean> | boolean;

/**
 * A node-wide exposure registry — from `node.expose()`. Serve it with
 * `node.serveExpose(expose, alpns)`.
 * @group Expose
 */
export class Expose implements Disposable {
  private readonly authorizers = new Map<string, ExposeAuthorizer>();
  private readonly table: Upcalls;
  private readonly owned: boolean;
  private readonly remove: () => void;

  /** @internal */
  constructor(
    /** @internal */
    readonly native: NativeExpose,
    upcalls?: Upcalls,
  ) {
    const t = adapterTable(upcalls);
    this.table = t.upcalls;
    this.owned = t.owned;
    try {
      this.remove = this.table.handle('expose_authorize', (async (
        req: { serviceId: string; peerId: string; metadata: Buffer },
        ctx: UpcallContext,
      ) => {
        const authorize = this.authorizers.get(req.serviceId);
        if (!authorize) throw new Error(`service ${req.serviceId} is not exposed`);
        const allowed = await authorize(
          { serviceId: req.serviceId, peer: req.peerId as NodeId, metadata: new Uint8Array(req.metadata) },
          ctx,
        );
        return { allowed: allowed === true };
      }) as never);
    } catch (e) {
      if (this.owned) this.table.close();
      throw e;
    }
  }

  /** The table authorizations are answered on. */
  get upcalls(): Upcalls {
    return this.table;
  }

  /**
   * Expose the local TCP service at `addr` (`"127.0.0.1:8080"`) as
   * `serviceId`, admitting the peers `authorize` accepts (asked once per
   * connection and service).
   */
  exposeSocket(serviceId: string, addr: string, authorize: ExposeAuthorizer): void {
    this.native.exposeSocket(serviceId, addr, this.table.native);
    this.authorizers.set(serviceId, authorize);
  }

  /** Stop exposing `serviceId`. */
  unexpose(serviceId: string): void {
    this.native.unexpose(serviceId);
    this.authorizers.delete(serviceId);
  }

  /** Remove the authorization handler (and close a private table). */
  close(): void {
    this.remove();
    if (this.owned) this.table.close();
  }

  [Symbol.dispose](): void {
    this.close();
  }
}
