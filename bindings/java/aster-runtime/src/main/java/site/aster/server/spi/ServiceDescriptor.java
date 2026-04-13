package site.aster.server.spi;

import site.aster.annotations.Scope;

/**
 * Identity and lifetime metadata for a service exposed by a {@link ServiceDispatcher}.
 *
 * @param name service name as published in the contract manifest
 * @param version service version
 * @param scope {@link Scope#SHARED} (one singleton per server) or {@link Scope#SESSION} (one
 *     instance per client connection)
 * @param implClass the user's annotated implementation class — the runtime uses this to match a
 *     registered instance (or factory, for SESSION) against a discovered dispatcher
 */
public record ServiceDescriptor(String name, int version, Scope scope, Class<?> implClass) {}
