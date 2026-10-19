package site.aster.handle;

import static org.junit.jupiter.api.Assertions.*;

import java.lang.ref.Cleaner;
import java.util.concurrent.*;
import java.util.concurrent.atomic.AtomicInteger;
import org.junit.jupiter.api.Test;
import site.aster.testing.WaitBudget;

class IrohHandleCleanupTest {
  static class Handle extends IrohHandle {
    final AtomicInteger released;

    Handle(AtomicInteger released) {
      super(42, id -> released.incrementAndGet());
      this.released = released;
    }

    protected String freeNativeKind() {
      return "test";
    }

    protected void freeNative(long id) {
      released.incrementAndGet();
    }
  }

  static Cleaner.Cleanable registration(IrohHandle handle) throws Exception {
    var field = IrohHandle.class.getDeclaredField("cleanable");
    field.setAccessible(true);
    return (Cleaner.Cleanable) field.get(handle);
  }

  @Test
  void cleanupAndConcurrentExplicitCloseAttemptReleaseExactlyOnce() throws Exception {
    AtomicInteger released = new AtomicInteger();
    Handle handle = new Handle(released);
    Cleaner.Cleanable cleanup = registration(handle);
    var pool = Executors.newFixedThreadPool(16);
    CountDownLatch start = new CountDownLatch(1);
    try {
      java.util.List<Future<?>> jobs = new java.util.ArrayList<>();
      for (int i = 0; i < 32; i++) {
        boolean explicit = i % 2 == 0;
        jobs.add(
            pool.submit(
                () -> {
                  try {
                    start.await();
                    if (explicit) handle.close();
                    else cleanup.clean();
                  } catch (InterruptedException e) {
                    Thread.currentThread().interrupt();
                    throw new RuntimeException(e);
                  }
                }));
      }
      start.countDown();
      for (Future<?> job : jobs) job.get(WaitBudget.SECONDS, TimeUnit.SECONDS);
      assertEquals(1, released.get());
      assertTrue(handle.isClosed());
      cleanup.clean();
      handle.close();
      assertEquals(1, released.get());
    } finally {
      start.countDown();
      pool.shutdownNow();
      assertTrue(pool.awaitTermination(WaitBudget.SECONDS, TimeUnit.SECONDS));
      handle.close();
    }
  }

  @Test
  void throwingExplicitReleaseStillClaimsExactlyOnce() throws Exception {
    AtomicInteger released = new AtomicInteger();
    RuntimeException failure = new RuntimeException("release failed");
    IrohHandle handle =
        new IrohHandle(42, id -> released.incrementAndGet()) {
          protected String freeNativeKind() {
            return "test";
          }

          protected void freeNative(long id) {
            released.incrementAndGet();
            throw failure;
          }
        };
    assertSame(failure, assertThrows(RuntimeException.class, handle::close));
    registration(handle).clean();
    handle.close();
    assertTrue(handle.isClosed());
    assertEquals(1, released.get());
  }

  @Test
  void originalConstructorRemainsExplicitOnlyAndIdempotent() throws Exception {
    AtomicInteger released = new AtomicInteger();
    IrohHandle handle =
        new IrohHandle(42) {
          protected String freeNativeKind() {
            return "test";
          }

          protected void freeNative(long id) {
            released.incrementAndGet();
          }
        };
    assertNull(registration(handle));
    handle.close();
    handle.close();
    assertEquals(1, released.get());
  }

  @Test
  void connectionCleanupCapturesOnlyPrimitiveRuntimeId() throws Exception {
    var factory = IrohConnection.class.getDeclaredMethod("cleanupAction", long.class);
    factory.setAccessible(true);
    Object action = factory.invoke(null, 123L);
    var fields = action.getClass().getDeclaredFields();
    assertTrue(fields.length > 0);
    for (var field : fields)
      assertTrue(
          field.getType().isPrimitive(), "Cleaner action must not retain connection/runtime graph");
  }
}
