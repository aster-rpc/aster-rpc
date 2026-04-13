package site.aster.server;

import java.lang.foreign.Arena;
import java.lang.foreign.MemorySegment;
import java.lang.foreign.ValueLayout;
import java.util.List;
import site.aster.codec.ForyCodec;
import site.aster.ffi.Reactor;
import site.aster.interceptors.RpcError;
import site.aster.interceptors.StatusCode;
import site.aster.server.spi.ResponseStream;
import site.aster.server.wire.RpcStatus;

/**
 * {@link ResponseStream} implementation that writes frames to the reactor on the server side. One
 * instance is created per in-flight streaming call — the generated dispatcher calls {@link #send}
 * for each response element and then {@link #complete} (or {@link #fail}) exactly once.
 *
 * <p>Not thread-safe by design: per the {@link ResponseStream} contract, the generated dispatcher
 * for a single call runs on a single virtual thread, so there is no concurrent access. The
 * reactor's FFI submit entry points are each called from one dispatcher thread at a time.
 */
final class ReactorResponseStream implements ResponseStream {

  private final Reactor reactor;
  private final long callId;
  private final ForyCodec headerCodec;
  private boolean terminated;

  ReactorResponseStream(Reactor reactor, long callId, ForyCodec headerCodec) {
    this.reactor = reactor;
    this.callId = callId;
    this.headerCodec = headerCodec;
  }

  @Override
  public void send(byte[] encoded) {
    if (terminated) {
      throw new IllegalStateException("send() after complete()/fail() for callId=" + callId);
    }
    byte[] frame = AsterFraming.encodeFrame(encoded, (byte) 0);
    try (Arena arena = Arena.ofConfined()) {
      MemorySegment seg = arena.allocate(ValueLayout.JAVA_BYTE, frame.length);
      seg.copyFrom(MemorySegment.ofArray(frame));
      reactor.submitFrame(callId, seg);
    }
  }

  @Override
  public void complete() {
    if (terminated) {
      return;
    }
    terminated = true;
    byte[] trailerPayload = headerCodec.encode(RpcStatus.ok());
    byte[] trailerFrame = AsterFraming.encodeFrame(trailerPayload, AsterFraming.FLAG_TRAILER);
    submitTrailerFrame(trailerFrame);
  }

  @Override
  public void fail(Throwable t) {
    if (terminated) {
      return;
    }
    terminated = true;
    StatusCode code;
    String message;
    if (t instanceof RpcError rpc) {
      code = rpc.code();
      message = rpc.rpcMessage();
    } else {
      code = StatusCode.INTERNAL;
      message = t.getMessage() == null ? t.getClass().getSimpleName() : t.getMessage();
    }
    RpcStatus status =
        new RpcStatus(code.value(), message == null ? "" : message, List.of(), List.of());
    byte[] trailerPayload = headerCodec.encode(status);
    byte[] trailerFrame = AsterFraming.encodeFrame(trailerPayload, AsterFraming.FLAG_TRAILER);
    submitTrailerFrame(trailerFrame);
  }

  @Override
  public boolean isCancelled() {
    // Cancellation propagation from the peer requires reading CANCEL frames off the request side
    // of a session stream — not wired yet, deferred alongside ClientStream / BidiStream support.
    return false;
  }

  private void submitTrailerFrame(byte[] trailerFrame) {
    try (Arena arena = Arena.ofConfined()) {
      MemorySegment seg = arena.allocate(ValueLayout.JAVA_BYTE, trailerFrame.length);
      seg.copyFrom(MemorySegment.ofArray(trailerFrame));
      reactor.submitTrailer(callId, seg);
    }
  }
}
