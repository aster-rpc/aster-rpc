package site.aster.server;

/**
 * Functional interface for handling incoming Aster RPC calls.
 *
 * <p>Implementations receive an {@link AsterCall} with the de-framed header and request payloads,
 * and must return a {@link CallResponse} containing the response and optional trailer bytes.
 *
 * <p>Handlers are invoked on virtual threads, so blocking operations are safe.
 */
@FunctionalInterface
public interface CallHandler {

  /**
   * Handle an incoming RPC call.
   *
   * @param call the incoming call descriptor
   * @return the response to send back to the caller
   * @throws Exception if handling fails; the server will send an error trailer
   */
  CallResponse handle(AsterCall call) throws Exception;
}
