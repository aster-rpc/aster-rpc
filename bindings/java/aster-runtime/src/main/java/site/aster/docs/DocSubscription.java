package site.aster.docs;

import java.lang.foreign.Arena;
import java.lang.foreign.ValueLayout;
import java.util.concurrent.CompletableFuture;
import site.aster.ffi.DataPlane;
import site.aster.ffi.IrohEventKind;
import site.aster.ffi.IrohException;
import site.aster.ffi.IrohLibrary;
import site.aster.ffi.IrohStatus;
import site.aster.handle.IrohRuntime;

/** Owns the two independent handles returned by join-and-subscribe. */
public record DocSubscription(Doc doc, DocEvents events) implements AutoCloseable {
  @Override
  public void close() {
    try {
      events.close();
    } finally {
      doc.close();
    }
  }

  static CompletableFuture<DocSubscription> join(IrohRuntime rt, long node, String ticket) {
    var result = new CompletableFuture<DocSubscription>();
    long operation;
    try (Arena a = Arena.ofConfined()) {
      var op = a.allocate(ValueLayout.JAVA_LONG);
      int status =
          IrohLibrary.getInstance()
              .docsJoinAndSubscribe(rt.nativeHandle(), node, DataPlane.utf8(a, ticket), 0L, op);
      if (status != 0)
        throw new IrohException(IrohStatus.fromCode(status), "join-and-subscribe failed");
      operation = op.get(ValueLayout.JAVA_LONG, 0);
    } catch (Throwable t) {
      return CompletableFuture.failedFuture(t);
    }
    rt.registry()
        .register(operation)
        .whenComplete(
            (event, error) -> {
              if (error != null) {
                result.completeExceptionally(error);
                return;
              }
              try {
                if (event.kind() != IrohEventKind.DOC_JOINED_AND_SUBSCRIBED)
                  throw new IrohException("unexpected join-and-subscribe event: " + event.kind());
                var owner =
                    new DocSubscription(
                        new Doc(rt, event.handle(), node), new DocEvents(rt, event.related()));
                if (!result.complete(owner)) owner.close();
              } catch (Throwable t) {
                result.completeExceptionally(t);
              } finally {
                if (event.hasBuffer()) rt.releaseBuffer(event.buffer());
              }
            });
    return result;
  }
}
