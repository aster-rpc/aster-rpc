package com.aster.tracker;

import java.lang.ref.Cleaner;
import java.util.*;
import java.util.concurrent.ConcurrentHashMap;
import java.util.concurrent.atomic.AtomicLong;

/**
 * Tracks outstanding native resources (handles, buffers) and verifies they are released.
 *
 * <p>Usage:
 *
 * <pre>{@code
 * LeakTracker tracker = new LeakTracker();
 * tracker.register(resourceId, () -> nativeFree(resourceId));
 * // ... use resource ...
 * tracker.unregister(resourceId); // called after nativeFree
 * tracker.assertClean(); // throws if any resource leaked
 * }</pre>
 */
public class LeakTracker {

  private final ConcurrentHashMap<Long, Cleaner.Cleanable> tracked = new ConcurrentHashMap<>();
  private final AtomicLong nextId = new AtomicLong(0);
  private final List<LeakedResource> leaked = Collections.synchronizedList(new ArrayList<>());

  private record LeakedResource(long id, String description) {}

  /**
   * Register a native resource for tracking. The {@code cleanup} runnable will be invoked when (a)
   * {@link #unregister(long)} is called, or (b) the LeakTracker is GC'd and the Cleaner runs —
   * whichever comes first.
   *
   * <p>Returns a unique {@code resourceId} that must be passed to {@link #unregister(long)} when
   * the resource is explicitly released.
   */
  public long register(Runnable cleanup, String description) {
    long id = nextId.incrementAndGet();
    Cleaner cleaner = Cleaner.create();
    Cleaner.Cleanable cleanable =
        cleaner.register(
            this,
            () -> {
              cleanup.run();
              leaked.add(new LeakedResource(id, description));
            });
    tracked.put(id, cleanable);
    return id;
  }

  /** Unregister a resource after native release. Calls the cleanup immediately. */
  public void unregister(long resourceId) {
    Cleaner.Cleanable cleanable = tracked.remove(resourceId);
    if (cleanable != null) {
      cleanable.clean();
    }
  }

  /** Returns the number of currently tracked resources. */
  public long trackedCount() {
    return tracked.size();
  }

  /**
   * Assert that all tracked resources have been released.
   *
   * @throws AssertionError if any resource was not explicitly unregistered
   */
  public void assertClean() {
    if (!tracked.isEmpty()) {
      throw new AssertionError(
          "LeakTracker: " + tracked.size() + " resource(s) not released: " + tracked.keySet());
    }
    if (!leaked.isEmpty()) {
      throw new AssertionError(
          "LeakTracker: " + leaked.size() + " resource(s) leaked (cleaner ran): " + leaked);
    }
  }

  /** Reset all state. Use between test cases. */
  public void reset() {
    tracked.clear();
    leaked.clear();
  }
}
