package site.aster.expose;

/**
 * Limits of one foreign HTTP service ({@code aster_http_foreign_options_t}); {@code 0} = the
 * default of each. See the upcall design, section 6.
 *
 * @param inlineRequestMax a request with a declared {@code content-length} at most this travels
 *     inline in the upcall (default 64 KiB)
 * @param maxRequestBody cap on the request body: a declared length over it is answered {@code 413}
 *     without an upcall, a streamed body that grows past it fails the read with {@link
 *     HttpBodyException.Kind#TOO_LARGE} (0 = unbounded)
 * @param maxResponseBody cap on the response body: an inline body over it becomes {@code 502}, a
 *     streamed one is aborted by the write that crosses it (0 = unbounded)
 * @param maxChunk largest single body write and most one read returns (default 1 MiB)
 * @param responseBuffer response chunks queued before a write waits (default 8)
 */
public record ExposeHttpOptions(
    long inlineRequestMax,
    long maxRequestBody,
    long maxResponseBody,
    long maxChunk,
    int responseBuffer) {

  /** What {@code maxChunk == 0} means. */
  public static final long DEFAULT_MAX_CHUNK = 1L << 20;

  public ExposeHttpOptions {
    if (inlineRequestMax < 0 || maxRequestBody < 0 || maxResponseBody < 0 || maxChunk < 0) {
      throw new IllegalArgumentException("limits must not be negative");
    }
    if (responseBuffer < 0) {
      throw new IllegalArgumentException("responseBuffer must not be negative");
    }
  }

  public static ExposeHttpOptions defaults() {
    return new ExposeHttpOptions(0, 0, 0, 0, 0);
  }

  public ExposeHttpOptions withInlineRequestMax(long v) {
    return new ExposeHttpOptions(v, maxRequestBody, maxResponseBody, maxChunk, responseBuffer);
  }

  public ExposeHttpOptions withMaxRequestBody(long v) {
    return new ExposeHttpOptions(inlineRequestMax, v, maxResponseBody, maxChunk, responseBuffer);
  }

  public ExposeHttpOptions withMaxResponseBody(long v) {
    return new ExposeHttpOptions(inlineRequestMax, maxRequestBody, v, maxChunk, responseBuffer);
  }

  public ExposeHttpOptions withMaxChunk(long v) {
    return new ExposeHttpOptions(
        inlineRequestMax, maxRequestBody, maxResponseBody, v, responseBuffer);
  }

  public ExposeHttpOptions withResponseBuffer(int v) {
    return new ExposeHttpOptions(inlineRequestMax, maxRequestBody, maxResponseBody, maxChunk, v);
  }

  long effectiveMaxChunk() {
    return maxChunk == 0 ? DEFAULT_MAX_CHUNK : maxChunk;
  }
}
