package site.aster.codec;

import org.apache.fory.BaseFory;
import org.apache.fory.Fory;
import org.apache.fory.ThreadSafeFory;
import org.apache.fory.config.ForyBuilder;

/**
 * Apache Fory 1.7.6 backed codec for RPC <em>payloads</em> (the envelope is AEv1, see {@link
 * site.aster.ffi.AsterEnvelope}). Backed by a {@link ThreadSafeFory} — Fory's {@code
 * ThreadPoolFory} — so a single {@code ForyCodec} instance can be shared across every in-flight
 * call on an {@code AsterServer} (which dispatches on a virtual-thread-per-call executor).
 *
 * <p>Options match Rust's {@code aster_transport_core::xlang::new_fory} and Aster-SPEC §5.1
 * (normative for every binding): xlang on, compatible mode on, reference tracking off. Different
 * options produce different bytes, so they must not drift.
 *
 * <p>See {@code docs/_internal/java-fory-threading.md} for why the pool (not {@code
 * ThreadLocalFory}) is used under virtual threads.
 *
 * <p>Type registration is the caller's responsibility — call {@link #fory()} to reach the
 * underlying {@link BaseFory} and register your types there. Registrations propagate to every
 * pooled {@code Fory} instance via Fory's {@code SharedRegistry}, so new pool slots created after
 * registration still see the right types.
 */
public final class ForyCodec implements Codec {

  private final ThreadSafeFory fory;

  /** Pinned Apache Fory release (Aster-SPEC §5.1; Rust {@code xlang::FORY_VERSION}). */
  public static final String FORY_VERSION = "1.7.6";

  public ForyCodec() {
    // A true shared pool across threads / virtual threads (ThreadLocalFory would build a fresh
    // Fory + codegen per virtual thread). Sized to CPU/2, at least 2.
    int cpu = Math.max(Runtime.getRuntime().availableProcessors(), 2);
    int poolSize = Math.max((cpu + 1) / 2, 2);
    this.fory = newBuilder().buildThreadSafeForyPool(poolSize);
  }

  /**
   * A {@link ForyBuilder} preconfigured with Aster's standard options (Aster-SPEC §5.1, Rust {@code
   * xlang::new_fory}): xlang, compatible mode, no reference tracking. Strict registration is kept
   * so unregistered classes fail at encode time.
   */
  public static ForyBuilder newBuilder() {
    return Fory.builder()
        .withXlang(true)
        .withCompatible(true)
        .withRefTracking(false)
        .requireClassRegistration(true);
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
