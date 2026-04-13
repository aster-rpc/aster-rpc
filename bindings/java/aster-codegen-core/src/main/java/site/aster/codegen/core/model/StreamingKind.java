package site.aster.codegen.core.model;

/** RPC shape classification — mirrors {@code site.aster.server.spi.StreamingKind}. */
public enum StreamingKind {
  UNARY,
  SERVER_STREAM,
  CLIENT_STREAM,
  BIDI_STREAM
}
