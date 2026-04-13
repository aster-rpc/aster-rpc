package site.aster.benchmarks;

import java.lang.foreign.Arena;
import java.lang.foreign.MemoryLayout;
import java.lang.foreign.MemorySegment;
import java.lang.foreign.ValueLayout;
import java.util.concurrent.TimeUnit;
import org.openjdk.jmh.annotations.Benchmark;
import org.openjdk.jmh.annotations.BenchmarkMode;
import org.openjdk.jmh.annotations.Fork;
import org.openjdk.jmh.annotations.Measurement;
import org.openjdk.jmh.annotations.Mode;
import org.openjdk.jmh.annotations.OutputTimeUnit;
import org.openjdk.jmh.annotations.Scope;
import org.openjdk.jmh.annotations.Setup;
import org.openjdk.jmh.annotations.State;
import org.openjdk.jmh.annotations.TearDown;
import org.openjdk.jmh.annotations.Warmup;
import site.aster.ffi.IrohLibrary;

/**
 * Java JMH benchmarks for FFI binding overhead.
 *
 * <p>Benchmarks measure the Java-side overhead of FFI calls.
 *
 * <p>Run with: mvn benchmark -Dbenchmark=IrohBenchmark
 *
 * <p>Requires JMH plugin and Java 25+ with FFM support.
 */
@State(Scope.Thread)
@BenchmarkMode(Mode.AverageTime)
@OutputTimeUnit(TimeUnit.NANOSECONDS)
@Warmup(iterations = 3, time = 1, timeUnit = TimeUnit.SECONDS)
@Measurement(iterations = 5, time = 1, timeUnit = TimeUnit.SECONDS)
@Fork(1)
public class IrohBenchmark {

  private static final IrohLibrary LIB = IrohLibrary.getInstance();

  // ─── Benchmark state ────────────────────────────────────────────────────

  @State(Scope.Benchmark)
  public static class BenchmarkState {
    public long runtimeHandle;
    public Arena arena;

    @Setup
    public void setup() {
      arena = Arena.ofConfined();
      // Create runtime via FFI
      // Note: We can't call iroh_runtime_new directly from here without JNI
      // These benchmarks validate Java FFM overhead, not the full FFI round-trip
    }

    @TearDown
    public void tearDown() {
      arena.close();
    }
  }

  // ─── Benchmarks ────────────────────────────────────────────────────────

  /** Measure overhead of Arena.allocate(IROH_EVENT) — zero allocation, just booking. */
  @Benchmark
  public void benchArenaAllocateEvent(BenchmarkState state) {
    // Just measuring arena allocation (no native call)
    MemorySegment seg = state.arena.allocate(IrohLibrary.IROH_EVENT);
    // Touch the segment to prevent optimization
    seg.set(ValueLayout.JAVA_BYTE, 0, (byte) 0);
  }

  /** Measure overhead of MemorySegment.set(ValueLayout, offset, value). */
  @Benchmark
  public void benchEventSegmentWrite(BenchmarkState state) {
    MemorySegment seg = state.arena.allocate(IrohLibrary.IROH_EVENT);
    seg.set(ValueLayout.JAVA_INT, 0, 80); // struct_size
    seg.set(ValueLayout.JAVA_INT, 4, 2); // kind
    seg.set(ValueLayout.JAVA_LONG, 16, 42); // operation
    seg.set(ValueLayout.JAVA_LONG, 24, 7); // handle
  }

  /** Measure overhead of MemorySegment.get(ValueLayout, offset). */
  @Benchmark
  public void benchEventSegmentRead(BenchmarkState state) {
    MemorySegment seg = state.arena.allocate(IrohLibrary.IROH_EVENT);
    seg.set(ValueLayout.JAVA_INT, 0, 80);
    seg.set(ValueLayout.JAVA_INT, 4, 2);
    seg.set(ValueLayout.JAVA_LONG, 16, 42);
    seg.set(ValueLayout.JAVA_LONG, 24, 7);

    // Read back
    long structSize = seg.get(ValueLayout.JAVA_INT, 0);
    long kind = seg.get(ValueLayout.JAVA_INT, 4);
    long operation = seg.get(ValueLayout.JAVA_LONG, 16);
    long handle = seg.get(ValueLayout.JAVA_LONG, 24);
  }

  /**
   * Measure overhead of MemoryLayout.PathElement.groupElement(). This is the lookup cost for field
   * access.
   */
  @Benchmark
  public void benchFieldOffsetLookup() {
    long offset =
        IrohLibrary.IROH_EVENT.byteOffset(MemoryLayout.PathElement.groupElement("operation"));
  }

  /**
   * Measure overhead of Arena.ofConfined() — creating an arena. Note: We can't measure this in
   * isolation since Arena is AutoCloseable.
   */
  @Benchmark
  public Arena benchArenaCreate() {
    Arena arena = Arena.ofConfined();
    arena.close();
    return arena;
  }

  /** Measure overhead of Arena.close(). */
  @Benchmark
  public void benchArenaClose() {
    Arena arena = Arena.ofConfined();
    arena.close();
  }
}
