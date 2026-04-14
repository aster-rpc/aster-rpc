import { WireType } from '@aster-rpc/aster';
import type { i32, i64 } from '@aster-rpc/aster';

@WireType('sample/StatusRequest')
export class StatusRequest {
  agentId = '';
  nonce: i64 = 0n as i64;
}

@WireType('sample/StatusResponse')
export class StatusResponse {
  status = '';
  uptime: i32 = 0 as i32;
  warnings: string[] = [];
}

@WireType('sample/WatchRequest')
export class WatchRequest {
  agentId = '';
  includeWarnings = false;
}

@WireType('sample/StatusEvent')
export class StatusEvent {
  at = new Date();
  status: StatusResponse = new StatusResponse();
  optionalNote?: string;
}
