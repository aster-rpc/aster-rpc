import { Service, Rpc, i32 } from '@aster-rpc/aster';
import { EchoRequest, EchoReply } from './types.js';

/** `message` and `data` echoed, `count + 1` (i32 wrap), `server_lang` = "ts". */
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
}
