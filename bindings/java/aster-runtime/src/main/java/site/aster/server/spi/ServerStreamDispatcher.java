package site.aster.server.spi;

import site.aster.codec.Codec;
import site.aster.interceptors.CallContext;

/** Dispatcher for a server-streaming ({@code @ServerStream}) method. */
public non-sealed interface ServerStreamDispatcher extends MethodDispatcher {

  /**
   * Invoke the user method and drain its output stream into {@code out}. Returns normally when the
   * user method has yielded its final value; the runtime is responsible for sending the trailing
   * status frame. Must call {@code out.fail(...)} on exceptional termination.
   */
  void invoke(Object impl, byte[] requestBytes, Codec codec, CallContext ctx, ResponseStream out)
      throws Exception;
}
