import { Service, Rpc, ServerStream, ClientStream, BidiStream, CallContext, i32 } from '@aster-rpc/aster';
import { EchoRequest, EchoReply } from './types.js';

/** `hold` calls that observed a cancellation (process-wide). */
let holdCancelled = 0;

/**
 * `echo`: `message` and `data` echoed, `count + 1` (i32 wrap), `server_lang` = "ts".
 * `generate`: `max(count, 0)` replies, reply i = { message, count: i, data }.
 * `ingest`: one reply, count = number of requests, message = last request's message.
 * `chat`: one reply per request, right away: request echoed, count = its 0-based position.
 * `hold`: blocks until cancelled (counted) or 60 s (replies count -1, "timeout").
 * `cancelled`: count = number of `hold` calls that observed a cancellation.
 * See tests/interop/rust/contract.rs.
 */
@Service({ name: 'InteropEcho', version: 1 })
export class InteropEcho {
  @Rpc()
  async echo(req: EchoRequest): Promise<EchoReply> {
    return new EchoReply({
      message: req.message,
      count: i32((req.count + 1) | 0),
      data: req.data,
      server_lang: 'ts',
    });
  }

  @ServerStream()
  async *generate(req: EchoRequest): AsyncGenerator<EchoReply> {
    for (let i = 0; i < Math.max(req.count, 0); i++) {
      yield new EchoReply({ message: req.message, count: i32(i), data: req.data, server_lang: 'ts' });
    }
  }

  @ClientStream()
  async ingest(reqs: AsyncIterable<EchoRequest>): Promise<EchoReply> {
    let n = 0;
    let last = '';
    for await (const r of reqs) {
      n++;
      last = r.message;
    }
    return new EchoReply({ message: last, count: i32(n | 0), data: new Uint8Array(), server_lang: 'ts' });
  }

  @BidiStream()
  async *chat(reqs: AsyncIterable<EchoRequest>): AsyncGenerator<EchoReply> {
    let i = 0;
    for await (const r of reqs) {
      yield new EchoReply({ message: r.message, count: i32(i), data: r.data, server_lang: 'ts' });
      i = (i + 1) | 0;
    }
  }

  @Rpc()
  async hold(_req: EchoRequest, ctx: CallContext): Promise<EchoReply> {
    const timedOut = await new Promise<boolean>((resolve) => {
      const timer = setTimeout(() => resolve(true), 60_000);
      const onAbort = () => {
        clearTimeout(timer);
        resolve(false);
      };
      if (ctx.signal.aborted) onAbort();
      else ctx.signal.addEventListener('abort', onAbort, { once: true });
    });
    if (!timedOut) holdCancelled++;
    return new EchoReply({ message: 'timeout', count: i32(-1), data: new Uint8Array(), server_lang: 'ts' });
  }

  @Rpc()
  async cancelled(_req: EchoRequest): Promise<EchoReply> {
    return new EchoReply({
      message: 'cancelled',
      count: i32(holdCancelled),
      data: new Uint8Array(),
      server_lang: 'ts',
    });
  }
}

/** Session-scoped: one instance per (connection, session id). */
@Service({ name: 'InteropSession', version: 1, scoped: 'session' })
export class InteropSession {
  private total = 0;
  private calls = 0;

  @Rpc()
  async add(req: EchoRequest): Promise<EchoReply> {
    this.total = (this.total + req.count) | 0;
    this.calls++;
    return new EchoReply({
      message: `calls=${this.calls}`,
      count: i32(this.total),
      data: new Uint8Array(),
      server_lang: 'ts',
    });
  }
}
