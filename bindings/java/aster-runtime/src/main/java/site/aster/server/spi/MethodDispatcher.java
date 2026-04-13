package site.aster.server.spi;

/**
 * Per-method dispatch contract. Sealed on four subtypes — one per {@link StreamingKind}. The
 * runtime dispatch loop does an exhaustive {@code switch} on these subtypes; the codegen layer
 * emits exactly one subtype per method.
 */
public sealed interface MethodDispatcher
    permits UnaryDispatcher, ServerStreamDispatcher, ClientStreamDispatcher, BidiStreamDispatcher {

  /** The method metadata, pre-computed at build time. */
  MethodDescriptor descriptor();
}
