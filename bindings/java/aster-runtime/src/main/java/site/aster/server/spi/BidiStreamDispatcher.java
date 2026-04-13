package site.aster.server.spi;

import site.aster.codec.Codec;
import site.aster.interceptors.CallContext;

/** Dispatcher for a bidirectional-streaming ({@code @BidiStream}) method. */
public non-sealed interface BidiStreamDispatcher extends MethodDispatcher {

  /**
   * Invoke the user method, bridging both request and response streams. Returns normally when the
   * user method has completed; the runtime sends the trailing status frame.
   */
  void invoke(Object impl, RequestStream in, Codec codec, CallContext ctx, ResponseStream out)
      throws Exception;
}
