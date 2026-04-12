package com.aster.interceptors;

/**
 * Per-call compression override interceptor.
 *
 * <p>Injects {@code _aster_compress_threshold} and {@code _aster_compress_enabled} into the call
 * context metadata so downstream transport code can honour per-call compression settings.
 */
public final class CompressionInterceptor implements Interceptor {

  /** Default compression threshold in bytes. */
  public static final int DEFAULT_THRESHOLD = 4096;

  private final int threshold;
  private final boolean enabled;

  /** Creates a compression interceptor with default threshold (4096) and enabled. */
  public CompressionInterceptor() {
    this(DEFAULT_THRESHOLD, true);
  }

  /**
   * Creates a compression interceptor.
   *
   * @param threshold payload size in bytes above which compression is applied. Set to -1 to disable
   *     regardless of payload size.
   * @param enabled master switch; when false, compression is suppressed for every call
   */
  public CompressionInterceptor(int threshold, boolean enabled) {
    this.threshold = threshold;
    this.enabled = enabled;
  }

  @Override
  public Object onRequest(CallContext ctx, Object request) {
    int effectiveThreshold = enabled ? threshold : -1;
    ctx.metadata().put("_aster_compress_threshold", String.valueOf(effectiveThreshold));
    ctx.metadata().put("_aster_compress_enabled", String.valueOf(enabled));
    return request;
  }
}
