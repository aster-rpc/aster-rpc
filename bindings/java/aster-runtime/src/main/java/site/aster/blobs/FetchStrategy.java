package site.aster.blobs;

/**
 * How a multi-provider fetch decomposes a HashSeq ({@code aster_fetch_strategy_t}). The provider
 * list is walked in order with failover in both variants.
 */
public enum FetchStrategy {
  /** One request; providers tried strictly in order. */
  WHOLE(0),
  /**
   * Root, then each child as its own request (up to 32 concurrently); the order applies per child,
   * not fleet-wide. Needs {@link BlobFormat#HASH_SEQ}.
   */
  SPLIT_CHILDREN(1);

  public final int code;

  FetchStrategy(int code) {
    this.code = code;
  }
}
