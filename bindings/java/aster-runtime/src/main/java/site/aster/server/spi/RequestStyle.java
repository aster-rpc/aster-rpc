package site.aster.server.spi;

/**
 * Classifies how a handler's request parameters are presented on the wire.
 *
 * <p>{@link #EXPLICIT}: the user method takes a single {@code @WireType} request class directly.
 * The wire type is that class verbatim.
 *
 * <p>{@link #INLINE}: the user method takes zero or more inline parameters. The codegen layer
 * synthesizes a {@code {MethodName}Request} record containing those parameters and the runtime
 * decodes that synthesized type before unpacking into positional arguments. An empty parameter list
 * (a "no input" method) is still {@code INLINE} with an empty synthesized record — matches the
 * Python reference.
 */
public enum RequestStyle {
  EXPLICIT,
  INLINE
}
