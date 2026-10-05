package site.aster.otel;

/** {@code aster_span_kind_t}. */
public enum SpanKind {
  INTERNAL(0),
  SERVER(1),
  CLIENT(2),
  PRODUCER(3),
  CONSUMER(4);

  final int code;

  SpanKind(int code) {
    this.code = code;
  }
}
