package site.aster.server.spi;

import site.aster.codec.Codec;
import site.aster.interceptors.CallContext;

/** Dispatcher for a unary ({@code @Rpc}) method. */
public non-sealed interface UnaryDispatcher extends MethodDispatcher {

  /**
   * Invoke the user method.
   *
   * @param impl the user's service instance
   * @param requestBytes the decoded request frame payload (not yet deserialized)
   * @param codec the codec to use for request/response conversion
   * @param ctx the call context; also published via {@link CallContext#current()} for the duration
   *     of the call
   * @return the encoded response bytes, ready to write as a single response frame
   */
  byte[] invoke(Object impl, byte[] requestBytes, Codec codec, CallContext ctx) throws Exception;
}
