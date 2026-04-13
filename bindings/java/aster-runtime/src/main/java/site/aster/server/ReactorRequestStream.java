package site.aster.server;

import site.aster.ffi.Reactor;
import site.aster.server.spi.RequestStream;

/**
 * {@link RequestStream} backed by the reactor's per-call request channel. Replays the inline first
 * request frame (delivered in the initial poll() descriptor) and then pulls subsequent frames via
 * {@link Reactor#recvFrame(long, int)} until end-of-stream.
 *
 * <p>Used by {@link AsterServer} for {@code @ClientStream} and {@code @BidiStream} dispatchers.
 * Single-consumer per call — the user method drains it sequentially. End-of-stream is signalled by
 * {@link #receive()} returning {@code null}.
 */
public final class ReactorRequestStream implements RequestStream {

  /**
   * Poll budget in milliseconds for each underlying {@code recvFrame} call. Small enough that the
   * dispatcher thread stays responsive (e.g. for future cancellation) but large enough that we
   * don't burn CPU spinning. Each timeout just retries.
   */
  private static final int POLL_TIMEOUT_MS = 1000;

  private final Reactor reactor;
  private final long callId;
  private byte[] firstFrame;
  private boolean firstDelivered;
  private boolean ended;

  public ReactorRequestStream(Reactor reactor, long callId, byte[] firstFramePayload) {
    this.reactor = reactor;
    this.callId = callId;
    this.firstFrame = firstFramePayload;
  }

  @Override
  public byte[] receive() {
    if (ended) {
      return null;
    }
    if (!firstDelivered) {
      firstDelivered = true;
      byte[] payload = firstFrame;
      firstFrame = null;
      return payload;
    }
    while (true) {
      Reactor.RecvFrame result = reactor.recvFrame(callId, POLL_TIMEOUT_MS);
      switch (result) {
        case Reactor.RecvFrame.Ok ok -> {
          return ok.payload();
        }
        case Reactor.RecvFrame.EndOfStream ignored -> {
          ended = true;
          return null;
        }
        case Reactor.RecvFrame.Timeout ignored -> {
          // retry
        }
      }
    }
  }
}
