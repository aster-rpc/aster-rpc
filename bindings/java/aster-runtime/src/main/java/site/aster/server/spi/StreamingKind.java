package site.aster.server.spi;

/** The four RPC shapes supported by Aster. */
public enum StreamingKind {
  UNARY,
  SERVER_STREAM,
  CLIENT_STREAM,
  BIDI_STREAM
}
