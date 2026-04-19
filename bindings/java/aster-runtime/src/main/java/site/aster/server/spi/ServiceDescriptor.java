package site.aster.server.spi;

import site.aster.annotations.Scope;
import site.aster.contract.CapabilityRequirement;

/**
 * Identity and lifetime metadata for a service exposed by a {@link ServiceDispatcher}.
 *
 * @param name service name as published in the contract manifest
 * @param version service version
 * @param scope {@link Scope#SHARED} (one singleton per server) or {@link Scope#SESSION} (one
 *     instance per client connection)
 * @param implClass the user's annotated implementation class — the runtime uses this to match a
 *     registered instance (or factory, for SESSION) against a discovered dispatcher
 * @param requires service-level capability baseline emitted from a class-level {@code @Requires};
 *     {@code null} means no service-wide gate. Checked before the method-level requirement.
 */
public record ServiceDescriptor(
    String name, int version, Scope scope, Class<?> implClass, CapabilityRequirement requires) {

  /** Legacy constructor for callers not yet supplying a {@code requires} argument. */
  public ServiceDescriptor(String name, int version, Scope scope, Class<?> implClass) {
    this(name, version, scope, implClass, null);
  }
}
