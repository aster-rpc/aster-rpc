package site.aster.codec;

import org.apache.fory.BaseFory;
import org.apache.fory.Fory;
import org.apache.fory.ThreadSafeFory;
import org.apache.fory.config.Language;

/**
 * Apache Fory v0.16 backed codec. Backed by a {@link ThreadSafeFory} — specifically Fory's {@code
 * ThreadPoolFory}, the default thread-safe implementation — so a single {@code ForyCodec} instance
 * can be shared across every in-flight call on an {@code AsterServer} (which dispatches on a
 * virtual-thread-per-call executor).
 *
 * <p>See {@code docs/_internal/java-fory-threading.md} for the full rationale, including why we
 * picked {@code ThreadPoolFory} over plain {@link Fory} (not thread-safe), {@code ThreadLocalFory}
 * (allocates a fresh {@code Fory} for every virtual thread), or pinning the server executor to
 * platform threads.
 *
 * <p>Type registration is the caller's responsibility — call {@link #fory()} to reach the
 * underlying {@link BaseFory} and register your types there. Registrations propagate to every
 * pooled {@code Fory} instance via Fory's {@code SharedRegistry}, so new pool slots created after
 * registration still see the right types.
 */
public final class ForyCodec implements Codec {

  private final ThreadSafeFory fory;

  public ForyCodec() {
    // NOTE: buildThreadSafeFory() in Fory 0.16 returns ThreadLocalFory, which allocates a fresh
    // Fory per thread on first use. Under newVirtualThreadPerTaskExecutor (every call = new VT)
    // that's a brand-new Fory + codec recompile on every RPC — ~200 µs per StreamHeader decode.
    // buildThreadSafeForyPool forces a ClassLoaderForyPooled, which is a true shared pool across
    // threads/VTs. Pool size is min=2, max=max(CPU/2, 2), matching Fory's own recommendation for
    // VT workloads.
    int cpu = Math.max(Runtime.getRuntime().availableProcessors(), 2);
    int max = Math.max(cpu / 2, 2);
    this.fory =
        Fory.builder()
            .withLanguage(Language.XLANG)
            // STRICT + XLANG, no ref tracking. Must match pyfory's defaults (xlang=True,
            // ref=False, strict=True) so the schema-hash Fory embeds in each XLANG payload
            // comes out byte-identical on both sides; any config drift here surfaces as
            // `Hash X is not consistent with Y for type T` on the receiver.
            .withRefTracking(false)
            .requireClassRegistration(true)
            .buildThreadSafeForyPool(2, max);
  }

  public ForyCodec(ThreadSafeFory fory) {
    this.fory = fory;
  }

  /**
   * The underlying thread-safe Fory instance. Register types via {@code fory().register(MyClass
   * .class, "fully.qualified.name")} before serializing them — registration propagates across all
   * pooled instances.
   */
  public BaseFory fory() {
    return fory;
  }

  @Override
  public String mode() {
    return "fory-xlang";
  }

  @Override
  public byte[] encode(Object value) {
    if (value == null) {
      return new byte[0];
    }
    return fory.serialize(value);
  }

  @Override
  public Object decode(byte[] payload, Class<?> type) {
    if (payload == null || payload.length == 0) {
      return null;
    }
    return fory.deserialize(payload);
  }
}
