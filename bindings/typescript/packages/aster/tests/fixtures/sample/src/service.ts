import { Service, Rpc, ServerStream, ClientStream, BidiStream, CallContext, anyOf, allOf } from '@aster-rpc/aster';
import { StatusRequest, StatusResponse, WatchRequest, StatusEvent } from './types.js';

/** Roles as an application declares them: `requires` names them by constant. */
const Role = { STATUS: 'ops.status', LOGS: 'ops.logs', ADMIN: 'ops.admin' } as const;

@Service({ name: 'MissionControl', version: 1 })
export class MissionControlService {
  @Rpc({ timeout: 30, idempotent: true })
  async getStatus(req: StatusRequest): Promise<StatusResponse> {
    const res = new StatusResponse();
    res.status = 'running';
    res.uptime = 42 as any;
    return res;
  }

  @Rpc({ requires: Role.STATUS })
  async getStatusWithCtx(req: StatusRequest, ctx: CallContext): Promise<StatusResponse> {
    void ctx;
    return new StatusResponse();
  }

  @ServerStream({ requires: anyOf(Role.LOGS, 'ops.admin') })
  async *watchStatus(req: WatchRequest): AsyncGenerator<StatusEvent> {
    void req;
    yield new StatusEvent();
  }

  @ClientStream({ requires: allOf(Role.STATUS, Role.ADMIN) })
  async ingestEvents(stream: AsyncIterable<StatusEvent>): Promise<StatusResponse> {
    for await (const _ of stream) void _;
    return new StatusResponse();
  }

  @BidiStream({ requires: 'ops.admin' })
  async *exchange(stream: AsyncIterable<StatusRequest>): AsyncGenerator<StatusEvent> {
    for await (const _ of stream) void _;
    yield new StatusEvent();
  }
}
