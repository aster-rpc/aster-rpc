package site.aster.server;

import site.aster.ffi.Dispatch;
import site.aster.server.spi.RequestStream;

/**
 * {@link RequestStream} over {@code aster_dispatch_recv}: replays the first request payload (from
 * the {@code CALL} event), then pulls the rest, already decompressed, until the end of the request
 * stream ({@code null}). Blocks in native code, so handlers using it run on platform threads.
 */
public final class DispatchRequestStream implements RequestStream {

  private final Dispatch dispatch;
  private final long callId;
  private byte[] first;
  private boolean ended;

  public DispatchRequestStream(Dispatch dispatch, long callId, byte[] firstPayload) {
    this.dispatch = dispatch;
    this.callId = callId;
    this.first = firstPayload;
  }

  @Override
  public byte[] receive() {
    if (ended) {
      return null;
    }
    if (first != null) {
      byte[] payload = first;
      first = null;
      return payload;
    }
    byte[] next = dispatch.recv(callId);
    if (next == null) {
      ended = true;
    }
    return next;
  }
}
