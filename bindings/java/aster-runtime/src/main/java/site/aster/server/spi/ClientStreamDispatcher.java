package site.aster.server.spi;

import site.aster.codec.Codec;
import site.aster.interceptors.CallContext;

/** Dispatcher for a client-streaming ({@code @ClientStream}) method. */
public non-sealed interface ClientStreamDispatcher extends MethodDispatcher {

  /**
   * Invoke the user method, consuming encoded frames from {@code in} and returning a single encoded
   * response. The user method is expected to drain {@code in} until it observes EOF.
   */
  byte[] invoke(Object impl, RequestStream in, Codec codec, CallContext ctx) throws Exception;
}
