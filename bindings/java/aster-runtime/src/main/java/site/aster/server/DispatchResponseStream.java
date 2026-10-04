package site.aster.server;

import site.aster.ffi.Dispatch;
import site.aster.interceptors.RpcError;
import site.aster.interceptors.StatusCode;
import site.aster.server.spi.ResponseStream;

/**
 * {@link ResponseStream} over {@code aster_dispatch_send} / {@code aster_dispatch_finish}: Rust
 * frames, compresses and writes; a full queue parks the producing thread (see {@link Dispatch}).
 * One instance per streaming call, used by one handler thread.
 */
final class DispatchResponseStream implements ResponseStream {

  private final Dispatch dispatch;
  private final long callId;
  private boolean terminated;

  DispatchResponseStream(Dispatch dispatch, long callId) {
    this.dispatch = dispatch;
    this.callId = callId;
  }

  @Override
  public void send(byte[] encoded) {
    if (terminated) {
      throw new IllegalStateException("send() after complete()/fail() for callId=" + callId);
    }
    if (dispatch.send(callId, encoded) == Dispatch.Outcome.GONE) {
      // Cancelled, past its deadline, or the peer went away: stop the generator.
      terminated = true;
      throw new RpcError(StatusCode.CANCELLED, "call " + callId + " is over");
    }
  }

  @Override
  public void complete() {
    if (terminated) {
      return;
    }
    terminated = true;
    dispatch.finish(callId, null);
  }

  @Override
  public void fail(Throwable t) {
    if (terminated) {
      return;
    }
    terminated = true;
    byte[] status =
        t instanceof RpcError rpc
            ? AsterServer.statusBytes(rpc.code(), rpc.rpcMessage(), rpc.details())
            : AsterServer.statusBytes(
                StatusCode.INTERNAL,
                t.getMessage() == null ? t.getClass().getSimpleName() : t.getMessage(),
                null);
    dispatch.finish(callId, status);
  }

  @Override
  public boolean isCancelled() {
    return dispatch.isCancelled(callId);
  }
}
