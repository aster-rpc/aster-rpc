package site.aster.ffi;

import java.lang.foreign.*;
import java.lang.invoke.MethodHandle;
import java.time.Duration;
import java.util.List;
import java.util.concurrent.CopyOnWriteArrayList;
import java.util.function.Consumer;
import java.util.function.LongConsumer;
import site.aster.event.IrohEvent;

/**
 * Dedicated platform thread that calls {@code aster_poll_events} and dispatches completions to the
 * {@link OperationRegistry} and registered inbound event handlers.
 *
 * <p>This must be a <b>platform thread</b>, not a virtual thread — it blocks on the native poll
 * call. Virtual threads must not block on native code in this design.
 */
public class IrohPollThread {

  private final OperationRegistry registry;
  private final long runtimeHandle;
  private final MemorySegment eventBuffer;
  private final int maxEvents;
  private final Duration pollTimeout;
  private final MethodHandle pollEvents;
  private final MethodHandle bufferRelease;

  private volatile boolean running = false;
  private volatile Thread thread;

  /** All registered inbound handlers. Each receives every inbound event. */
  private final List<Consumer<IrohEvent>> inboundHandlers = new CopyOnWriteArrayList<>();

  public IrohPollThread(OperationRegistry registry, long runtimeHandle) {
    this(registry, runtimeHandle, 64, Duration.ofMillis(100));
  }

  public IrohPollThread(
      OperationRegistry registry, long runtimeHandle, int maxEvents, Duration pollTimeout) {
    this(registry, runtimeHandle, maxEvents, pollTimeout, IrohLibrary.getInstance());
  }

  private IrohPollThread(
      OperationRegistry registry,
      long runtimeHandle,
      int maxEvents,
      Duration pollTimeout,
      IrohLibrary lib) {
    this(
        registry,
        runtimeHandle,
        maxEvents,
        pollTimeout,
        lib.allocator().allocate(IrohLibrary.ASTER_EVENT.byteSize() * maxEvents, 1),
        lib.getHandle(
            "aster_poll_events",
            FunctionDescriptor.of(
                ValueLayout.JAVA_LONG,
                ValueLayout.JAVA_LONG,
                ValueLayout.ADDRESS,
                ValueLayout.JAVA_LONG,
                ValueLayout.JAVA_INT)),
        lib.getHandle(
            "aster_buffer_release",
            FunctionDescriptor.of(
                ValueLayout.JAVA_INT, ValueLayout.JAVA_LONG, ValueLayout.JAVA_LONG)));
  }

  // Injection keeps shutdown/batch arbitration tests deterministic without native timing races.
  IrohPollThread(
      OperationRegistry registry,
      long runtimeHandle,
      int maxEvents,
      Duration pollTimeout,
      MemorySegment eventBuffer,
      MethodHandle pollEvents,
      MethodHandle bufferRelease) {
    this.registry = registry;
    this.runtimeHandle = runtimeHandle;
    this.maxEvents = maxEvents;
    this.pollTimeout = pollTimeout;
    this.eventBuffer = eventBuffer;
    this.pollEvents = pollEvents;
    this.bufferRelease = bufferRelease;
  }

  /** Add a handler for inbound events. Handlers are cumulative. */
  public void addInboundHandler(Consumer<IrohEvent> handler) {
    inboundHandlers.add(handler);
  }

  /** Remove an inbound handler. */
  public void removeInboundHandler(Consumer<IrohEvent> handler) {
    inboundHandlers.remove(handler);
  }

  public void start() {
    if (running) return;
    running = true;
    thread = new Thread(this::run, "iroh-poll");
    thread.setDaemon(true);
    thread.start();
  }

  public void stop() {
    running = false;
    Thread worker = thread;
    // Completion and inbound continuations execute here: self-stop must not interrupt or join them.
    if (worker != null && worker != Thread.currentThread()) {
      worker.interrupt();
      try {
        worker.join(Duration.ofSeconds(5).toMillis());
      } catch (InterruptedException ignored) {
        Thread.currentThread().interrupt();
      }
      if (!worker.isAlive() && thread == worker) thread = null;
    }
  }

  private void run() {
    while (running) {
      try {
        pollAndDispatch();
      } catch (InterruptedException ignored) {
        break;
      } catch (Throwable t) {
        System.err.println("iroh poll error: " + t.getMessage());
      }
    }
  }

  private void pollAndDispatch() throws Throwable {
    int timeoutMs = (int) pollTimeout.toMillis();
    long count = (long) pollEvents.invoke(runtimeHandle, eventBuffer, (long) maxEvents, timeoutMs);

    // A synchronous completion can close the runtime and free the rest of this native batch.
    for (int i = 0; i < count && running; i++) {
      MemorySegment eventSeg =
          eventBuffer.asSlice(
              i * IrohLibrary.ASTER_EVENT.byteSize(), IrohLibrary.ASTER_EVENT.byteSize());
      IrohEvent event = IrohEvent.fromSegment(eventSeg);
      dispatchEvent(event);
    }
  }

  private void dispatchEvent(IrohEvent event) {
    // Error events — always complete exceptionally
    if (event.kind() == IrohEventKind.ERROR || event.kind() == IrohEventKind.OPERATION_CANCELLED) {
      dispatchFailure(
          registry,
          event,
          buffer -> {
            try {
              bufferRelease.invoke(runtimeHandle, buffer);
            } catch (Throwable ignored) {
              // A closing native runtime owns any remaining buffers.
            }
          });
      return;
    }

    // Non-terminal inbound events — dispatch to inbound handlers only.
    // These are identified by having a handle but no operation future (the operation
    // already completed; these are async notifications).
    // SEND_COMPLETED is also inbound since it's tracked via pendingSends in IrohStream.
    if (event.kind() == IrohEventKind.FRAME_RECEIVED
        || event.kind() == IrohEventKind.SEND_COMPLETED
        || event.kind() == IrohEventKind.HOOK_BEFORE_CONNECT
        || event.kind() == IrohEventKind.HOOK_AFTER_CONNECT) {
      for (Consumer<IrohEvent> handler : inboundHandlers) {
        // A preceding handler can close the runtime and invalidate this event's borrowed payload.
        if (!running) break;
        try {
          handler.accept(event);
        } catch (Throwable t) {
          System.err.println("inbound handler error: " + t.getMessage());
        }
      }
      return;
    }

    // Terminal success events — complete the operation future.
    // STREAM_OPENED, STREAM_ACCEPTED, ENDPOINT_CREATED, CONNECTED,
    // CONNECTION_ACCEPTED, etc.
    registry.complete(event.operation(), event);
  }

  /** Error futures receive copied text only; the poller owns the borrowed native buffer. */
  static void dispatchFailure(OperationRegistry registry, IrohEvent event, LongConsumer release) {
    IrohStatus status = IrohStatus.fromCode(event.status());
    String detail;
    try {
      detail = readErrorPayload(event);
    } finally {
      if (event.hasBuffer()) release.accept(event.buffer());
    }
    String message =
        detail.isEmpty()
            ? "operation failed: " + status.name()
            : "operation failed: " + status.name() + ": " + detail;
    registry.completeExceptionally(
        event.operation(),
        IrohException.forStatus(status, message).withEvent(event.errorCode(), event.flags()));
  }

  public boolean isRunning() {
    return running;
  }

  private static String readErrorPayload(IrohEvent event) {
    if (event.dataLen() <= 0 || event.data() == null || event.data().address() == 0) {
      return "";
    }
    try {
      long len = Math.min(event.dataLen(), 4096L);
      byte[] bytes = event.data().asSlice(0, len).toArray(ValueLayout.JAVA_BYTE);
      return new String(bytes, java.nio.charset.StandardCharsets.UTF_8);
    } catch (Throwable t) {
      return "";
    }
  }
}
