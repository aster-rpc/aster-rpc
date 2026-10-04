package site.aster.coordination;

import java.util.concurrent.CompletableFuture;
import java.util.concurrent.atomic.AtomicBoolean;
import site.aster.coordination.Coordination.MutationReply;
import site.aster.handle.IrohRuntime;

/**
 * A mutation resolved and journaled ({@code aster_coordination_resolved_t}). Handle the {@link
 * #reply()} durably, then {@link #acknowledge()}: until then no new mutation is permitted. {@link
 * #close()} without acknowledging is safe and deliberate: the result stays journaled and {@code
 * CoordinationClient.unacknowledged()} redelivers it.
 */
public final class ResolvedMutation implements AutoCloseable {

  private final IrohRuntime runtime;
  private final long handle;
  private final MutationReply reply;
  private final AtomicBoolean consumed = new AtomicBoolean();

  ResolvedMutation(IrohRuntime runtime, long handle, MutationReply reply) {
    this.runtime = runtime;
    this.handle = handle;
    this.reply = reply;
  }

  public MutationReply reply() {
    return reply;
  }

  public long nativeHandle() {
    return handle;
  }

  /**
   * Acknowledge: the application durably handled this result; frees the journal for the next
   * sequence. <b>Consumes this handle</b> (a second call fails {@code NOT_FOUND}), also when it
   * fails with {@code CLOSED} because the client was closed: the result then stays journaled for
   * the next open. If the acknowledgement fails otherwise the result stays journaled and is
   * redelivered.
   */
  public CompletableFuture<Void> acknowledge() {
    if (!consumed.compareAndSet(false, true)) {
      return CompletableFuture.failedFuture(
          CoordinationExceptions.notFound("resolved mutation already consumed"));
    }
    return CoordNative.submit(
        runtime,
        "aster_coordination_resolved_acknowledge",
        (a, out) -> (int) CoordNative.ACKNOWLEDGE.invoke(runtime.nativeHandle(), handle, 0L, out),
        r -> null);
  }

  /** Drop without acknowledging (once); a no-op after {@link #acknowledge()}. */
  @Override
  public void close() {
    if (consumed.compareAndSet(false, true)) {
      try {
        CoordNative.RESOLVED_FREE.invoke(runtime.nativeHandle(), handle);
      } catch (Throwable ignored) {
        // runtime already closed
      }
    }
  }
}
