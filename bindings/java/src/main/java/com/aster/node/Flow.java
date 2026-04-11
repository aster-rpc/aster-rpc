package com.aster.node;

import java.util.concurrent.*;

/**
 * A reactive flow of values.
 *
 * <p>This is a simplified version of kotlinx.coroutines.flow.Flow for use in the Java FFI bindings.
 */
public interface Flow<T> {
  /**
   * Collect all values from this flow using the given collector.
   *
   * @param collector function to process each value
   * @param executor executor to run the collector on
   * @return a future that completes when collection finishes
   */
  default CompletableFuture<Void> collect(
      java.util.function.Consumer<T> collector, Executor executor) {
    return CompletableFuture.completedFuture(null);
  }
}
