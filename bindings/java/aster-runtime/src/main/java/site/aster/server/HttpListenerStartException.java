package site.aster.server;

/** Startup failed after binding and cleanup also failed; the listener remains owned for retry. */
public final class HttpListenerStartException extends RuntimeException {
  private final HttpListener listener;
  private final Throwable cleanupError;

  HttpListenerStartException(HttpListener listener, Throwable cause, Throwable cleanupError) {
    super("listener startup and cleanup failed; close listener() to retry", cause);
    this.listener = listener;
    this.cleanupError = cleanupError;
    addSuppressed(cleanupError);
  }

  /** The owner to close/retry. Address strings can be empty if address retrieval also failed. */
  public HttpListener listener() {
    return listener;
  }

  public Throwable cleanupError() {
    return cleanupError;
  }
}
