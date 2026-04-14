import { Service, Rpc, ServerStream, CallContext } from '@aster-rpc/aster';
import { StatusRequest, StatusResponse, WatchRequest, StatusEvent } from './types.js';

@Service({ name: 'MissionControl', version: 1 })
export class MissionControlService {
  @Rpc({ timeout: 30, idempotent: true })
  async getStatus(req: StatusRequest): Promise<StatusResponse> {
    const res = new StatusResponse();
    res.status = 'running';
    res.uptime = 42 as any;
    return res;
  }

  @Rpc()
  async getStatusWithCtx(req: StatusRequest, ctx: CallContext): Promise<StatusResponse> {
    void ctx;
    return new StatusResponse();
  }

  @ServerStream()
  async *watchStatus(req: WatchRequest): AsyncGenerator<StatusEvent> {
    void req;
    yield new StatusEvent();
  }
}
