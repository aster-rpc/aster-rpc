package site.aster.server.spi;

/**
 * Sink for response frames on a streaming dispatch. The dispatcher pushes already-encoded bytes;
 * the runtime is responsible for framing and submission to the reactor.
 *
 * <p>All methods are blocking. Generated dispatcher code runs on a virtual thread, so blocking is
 * cheap. Implementations MUST be safe for sequential single-threaded use by one dispatcher at a
 * time — they do NOT need to be thread-safe.
 */
public interface ResponseStream {

  /** Send one encoded response frame. */
  void send(byte[] encoded) throws Exception;

  /** Signal successful end-of-stream. The runtime sends an OK trailer. */
  void complete() throws Exception;

  /** Signal failed end-of-stream. The runtime sends an error trailer derived from {@code t}. */
  void fail(Throwable t) throws Exception;

  /**
   * Returns {@code true} if the peer has cancelled the call (e.g. by closing the stream or
   * signalling a CANCEL frame). Dispatchers polling a long-running source should check this
   * periodically and stop early.
   */
  boolean isCancelled();
}
