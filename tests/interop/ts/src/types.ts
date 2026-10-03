/**
 * Shared interop contract — must stay identical to
 * tests/interop/rust/interop_echo.rs and tests/interop/python/interop_echo.py.
 *
 *   interop/EchoRequest { message: string, count: varint32, data: binary }
 *   interop/EchoReply   { message: string, count: varint32, data: binary, server_lang: string }
 */
import { WireType, i32 } from '@aster-rpc/aster';

@WireType('interop/EchoRequest')
export class EchoRequest {
  message = '';
  count: i32 = i32(0);
  data: Uint8Array = new Uint8Array();
  constructor(init?: Partial<EchoRequest>) { if (init) Object.assign(this, init); }
}

@WireType('interop/EchoReply')
export class EchoReply {
  message = '';
  count: i32 = i32(0);
  data: Uint8Array = new Uint8Array();
  server_lang = '';
  constructor(init?: Partial<EchoReply>) { if (init) Object.assign(this, init); }
}
