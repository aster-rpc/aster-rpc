package site.aster.interceptors;

import static org.junit.jupiter.api.Assertions.assertEquals;
import static org.junit.jupiter.api.Assertions.assertSame;
import static org.junit.jupiter.api.Assertions.assertThrows;

import org.junit.jupiter.api.Test;

final class CallContextCurrentTest {

  @Test
  void currentThrowsOutsideScope() {
    assertThrows(IllegalStateException.class, CallContext::current);
  }

  @Test
  void runWithPublishesContext() throws Exception {
    CallContext ctx = CallContext.builder("svc", "m").peer("peer-1").build();
    String observed =
        CallContext.runWith(
            ctx,
            () -> {
              assertSame(ctx, CallContext.current());
              return CallContext.current().peer();
            });
    assertEquals("peer-1", observed);
  }

  @Test
  void runWithClearsAfterActionEvenOnException() {
    CallContext ctx = CallContext.builder("svc", "m").build();
    assertThrows(
        RuntimeException.class,
        () ->
            CallContext.runWith(
                ctx,
                () -> {
                  throw new RuntimeException("boom");
                }));
    assertThrows(IllegalStateException.class, CallContext::current);
  }

  @Test
  void runWithRestoresPriorContextOnNesting() throws Exception {
    CallContext outer = CallContext.builder("svc", "m1").callId("outer").build();
    CallContext inner = CallContext.builder("svc", "m2").callId("inner").build();

    CallContext.runWith(
        outer,
        () -> {
          assertEquals("outer", CallContext.current().callId());
          CallContext.runWith(
              inner,
              () -> {
                assertEquals("inner", CallContext.current().callId());
                return null;
              });
          assertEquals("outer", CallContext.current().callId());
          return null;
        });

    assertThrows(IllegalStateException.class, CallContext::current);
  }
}
