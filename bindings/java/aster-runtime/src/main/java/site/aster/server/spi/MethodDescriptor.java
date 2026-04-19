package site.aster.server.spi;

import java.util.List;
import site.aster.contract.CapabilityRequirement;

/**
 * Metadata describing one method on a {@link ServiceDispatcher}. Produced by the codegen layer and
 * consumed by (1) the manifest publisher and (2) the runtime dispatch loop.
 *
 * @param name method name on the wire (may differ from the Java method name if
 *     {@code @Rpc(name=...)} was set)
 * @param streaming the RPC shape
 * @param requestStyle whether the request came from an explicit {@code @WireType} class or a
 *     synthesized Mode 2 wrapper
 * @param requestType Fory xlang type tag for the request class (synthesized name for INLINE)
 * @param inlineParams positional parameter descriptors for INLINE; empty for EXPLICIT
 * @param responseType Fory xlang type tag for the response class
 * @param hasContextParam {@code true} if the user's method declared a trailing {@code CallContext}
 *     parameter to be injected at dispatch time
 * @param idempotent {@code true} if this method is safe to retry
 * @param requires capability requirement emitted from {@code @Requires}; {@code null} means the
 *     method is callable without any role (subject to the service-level requirement). Consumed by
 *     {@code site.aster.interceptors.CapabilityInterceptor} and published verbatim in the service
 *     contract's {@code MethodDef.requires}.
 */
public record MethodDescriptor(
    String name,
    StreamingKind streaming,
    RequestStyle requestStyle,
    String requestType,
    List<ParamDescriptor> inlineParams,
    String responseType,
    boolean hasContextParam,
    boolean idempotent,
    CapabilityRequirement requires) {

  public MethodDescriptor {
    inlineParams = List.copyOf(inlineParams);
  }

  /** Legacy constructor for callers not yet supplying a {@code requires} argument. */
  public MethodDescriptor(
      String name,
      StreamingKind streaming,
      RequestStyle requestStyle,
      String requestType,
      List<ParamDescriptor> inlineParams,
      String responseType,
      boolean hasContextParam,
      boolean idempotent) {
    this(
        name,
        streaming,
        requestStyle,
        requestType,
        inlineParams,
        responseType,
        hasContextParam,
        idempotent,
        null);
  }
}
