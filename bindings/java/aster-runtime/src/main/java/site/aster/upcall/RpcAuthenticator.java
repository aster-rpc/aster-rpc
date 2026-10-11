package site.aster.upcall;

/**
 * Per-call HTTP/WebTransport authentication. Return an explicit rejection or a resolved principal
 * with attributes for Gate 3. Exceptions fail with UNAVAILABLE; they never grant access.
 */
@FunctionalInterface
public interface RpcAuthenticator {
  /**
   * Runs on an upcall worker. The invocation supplies the native deadline and cancellation reason;
   * native cancellation also interrupts this thread. Do not swallow interrupts in blocking work.
   */
  Ucv1.RpcAuthResponse authenticate(Ucv1.RpcAuthRequest request, Upcalls.Invocation invocation)
      throws Exception;
}
