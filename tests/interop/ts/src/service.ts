import { Service, Rpc, ServerStream, ClientStream, i32 } from '@aster-rpc/aster';
import { EchoRequest, EchoReply } from './types.js';

/**
 * `echo`: `message` and `data` echoed, `count + 1` (i32 wrap), `server_lang` = "ts".
 * `generate`: `max(count, 0)` replies, reply i = { message, count: i, data }.
 * `ingest`: one reply, count = number of requests, message = last request's message.
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
}
