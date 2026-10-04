package site.aster.expose;

/**
 * A Java HTTP service for {@link Expose#exposeHttp}: one call per request (upcall kind {@code
 * expose_http}), on its own virtual thread, so blocking is fine. A thrown exception answers {@code
 * 500} (the message is logged by Rust, never sent), a timeout of the kind's policy {@code 504}. The
 * thread is interrupted when the consumer goes away before the response head.
 */
@FunctionalInterface
public interface HttpHandler {

  HttpResponse handle(HttpRequest request) throws Exception;
}
