package site.aster.handle;

import java.lang.foreign.*;
import java.lang.invoke.MethodHandle;
import java.util.concurrent.*;
import java.util.function.Consumer;
import site.aster.event.IrohEvent;
import site.aster.ffi.*;
import site.aster.tracker.LeakTracker;

/**
 * Wraps an {@code aster_runtime_t} handle.
 *
 * <p>Each {@code IrohRuntime} owns a Tokio runtime and event queue on the native side. There is
 * typically one per JVM process.
 */
public class IrohRuntime implements AutoCloseable {

  private final long handle;
  private final IrohLibrary lib;
  private final OperationRegistry registry;
  private final IrohPollThread poller;
  private final LeakTracker tracker;

  private final MethodHandle runtimeClose;
  private final MethodHandle bufferRelease;
  private final MethodHandle operationCancel;

  /** Create a new runtime using default library and leak tracker. */
  public static IrohRuntime create() {
    return new IrohRuntime(IrohLibrary.getInstance(), new LeakTracker());
  }

  IrohRuntime(IrohLibrary lib, LeakTracker tracker) {
    this.lib = lib;
    this.tracker = tracker;
    this.registry = new OperationRegistry();

    MemorySegment configSeg = lib.allocator().allocate(IrohLibrary.ASTER_RUNTIME_CONFIG);
    configSeg.set(ValueLayout.JAVA_INT, 0, (int) IrohLibrary.ASTER_RUNTIME_CONFIG.byteSize());
    configSeg.set(ValueLayout.JAVA_INT, 4, 0); // worker_threads = 0 (library default)
    configSeg.set(ValueLayout.JAVA_INT, 8, 0); // event_queue_capacity = 0 (library default)
    configSeg.set(ValueLayout.JAVA_INT, 12, 0); // reserved

    var runtimeNew =
        lib.getHandle(
            "aster_runtime_new",
            FunctionDescriptor.of(
                ValueLayout.JAVA_INT,
                ValueLayout.JAVA_INT,
                ValueLayout.ADDRESS,
                ValueLayout.ADDRESS));

    try {
      int status =
          (int)
              runtimeNew.invoke(
                  IrohLibrary.EXPECTED_ABI_MAJOR, configSeg, lib.runtimeHandleSegment());
      if (status != 0) {
        throw new IrohException(IrohStatus.fromCode(status), "aster_runtime_new failed: " + status);
      }
    } catch (IrohException e) {
      throw e;
    } catch (Throwable t) {
      throw new IrohException("aster_runtime_new threw: " + t.getMessage());
    }

    this.handle = lib.runtimeHandleSegment().get(ValueLayout.JAVA_LONG, 0);
    this.poller = new IrohPollThread(registry, handle);
    this.poller.start();

    this.runtimeClose =
        lib.getHandle(
            "aster_runtime_close",
            FunctionDescriptor.of(ValueLayout.JAVA_INT, ValueLayout.JAVA_LONG));

    this.bufferRelease =
        lib.getHandle(
            "aster_buffer_release",
            FunctionDescriptor.of(
                ValueLayout.JAVA_INT, ValueLayout.JAVA_LONG, ValueLayout.JAVA_LONG));

    this.operationCancel =
        lib.getHandle(
            "aster_operation_cancel",
            FunctionDescriptor.of(
                ValueLayout.JAVA_INT, ValueLayout.JAVA_LONG, ValueLayout.JAVA_LONG));
  }

  /** Returns the native runtime handle for FFI calls. */
  public long nativeHandle() {
    return handle;
  }

  /** Returns the operation registry for registering async operation futures. */
  public OperationRegistry registry() {
    return registry;
  }

  /** Release a native buffer leased from an event. */
  public void releaseBuffer(long bufferHandle) {
    int status = callSync(bufferRelease, handle, bufferHandle);
    if (status != 0) {
      throw new IrohException(IrohStatus.fromCode(status), "aster_buffer_release failed");
    }
  }

  /** Cancel an in-flight operation. */
  public void cancelOperation(long operationId) {
    int status = callSync(operationCancel, handle, operationId);
    if (status != 0) {
      throw new IrohException(IrohStatus.fromCode(status), "aster_operation_cancel failed");
    }
  }

  /** Register a handler for inbound events (frames, accepted connections, etc.). */
  public void addInboundHandler(Consumer<IrohEvent> handler) {
    poller.addInboundHandler(handler);
  }

  /**
   * Remove an inbound handler.
   *
   * @param handler the handler to remove
   */
  public void removeInboundHandler(Consumer<IrohEvent> handler) {
    poller.removeInboundHandler(handler);
  }

  /** Returns the operation registry for direct use. */
  public OperationRegistry operations() {
    return registry;
  }

  @Override
  public void close() {
    poller.stop();
    try {
      runtimeClose.invoke(handle);
    } catch (Throwable t) {
      throw new IrohException("aster_runtime_close failed: " + t.getMessage());
    }
  }

  private int callSync(MethodHandle mh, Object... args) {
    try {
      return (int) mh.invokeWithArguments(args);
    } catch (Throwable t) {
      throw new IrohException("FFI call failed: " + t.getMessage());
    }
  }
}
