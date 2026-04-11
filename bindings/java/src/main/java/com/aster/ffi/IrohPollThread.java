package com.aster.ffi;

import com.aster.event.IrohEvent;
import java.lang.foreign.*;
import java.lang.invoke.MethodHandle;
import java.time.Duration;
import java.util.List;
import java.util.concurrent.CopyOnWriteArrayList;
import java.util.function.Consumer;

/**
 * Dedicated platform thread that calls {@code iroh_poll_events} and dispatches completions to the
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

  private volatile boolean running = false;
  private Thread thread;

  /** All registered inbound handlers. Each receives every inbound event. */
  private final List<Consumer<IrohEvent>> inboundHandlers = new CopyOnWriteArrayList<>();

  public IrohPollThread(OperationRegistry registry, long runtimeHandle) {
    this(registry, runtimeHandle, 64, Duration.ofMillis(100));
  }

  public IrohPollThread(
      OperationRegistry registry, long runtimeHandle, int maxEvents, Duration pollTimeout) {
    this.registry = registry;
    this.runtimeHandle = runtimeHandle;
    this.maxEvents = maxEvents;
    this.pollTimeout = pollTimeout;

    IrohLibrary lib = IrohLibrary.getInstance();

    this.eventBuffer = lib.allocator().allocate(IrohLibrary.IROH_EVENT.byteSize() * maxEvents, 1);

    this.pollEvents =
        lib.getHandle(
            "iroh_poll_events",
            FunctionDescriptor.of(
                ValueLayout.JAVA_LONG,
                ValueLayout.JAVA_LONG,
                ValueLayout.ADDRESS,
                ValueLayout.JAVA_LONG,
                ValueLayout.JAVA_INT));
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
    if (thread != null) {
      thread.interrupt();
      try {
        thread.join(Duration.ofSeconds(5).toMillis());
      } catch (InterruptedException ignored) {
      }
      thread = null;
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

    for (int i = 0; i < count; i++) {
      MemorySegment eventSeg =
          eventBuffer.asSlice(
              i * IrohLibrary.IROH_EVENT.byteSize(), IrohLibrary.IROH_EVENT.byteSize());
      IrohEvent event = IrohEvent.fromSegment(eventSeg);
      dispatchEvent(event);
    }
  }

  private void dispatchEvent(IrohEvent event) {
    // Error events — always complete exceptionally
    if (event.kind() == IrohEventKind.ERROR || event.kind() == IrohEventKind.OPERATION_CANCELLED) {
      IrohStatus status = IrohStatus.fromCode(event.status());
      registry.completeExceptionally(
          event.operation(), IrohException.forStatus(status, "operation failed: " + status.name()));
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

  public boolean isRunning() {
    return running;
  }
}
