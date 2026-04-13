package site.aster.client;

import java.util.concurrent.Executor;
import java.util.concurrent.Executors;

/**
 * Shared executor for FFI blocking operations. The {@code aster_call_*} entry points block on the
 * calling thread while they drive tokio's {@code block_on} — running them on virtual threads keeps
 * the per-call cost tiny (no platform-thread pool saturation) while still letting each call's
 * send/recv be a straight-line synchronous sequence inside the task.
 */
final class CallExecutor {

  static final Executor INSTANCE =
      Executors.newThreadPerTaskExecutor(Thread.ofVirtual().name("aster-call-", 0).factory());

  private CallExecutor() {}
}
