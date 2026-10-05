package site.aster.benchmarks;

import java.time.Duration;
import java.util.Map;
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
import site.aster.otel.Metrics;
import site.aster.otel.Observability;
import site.aster.otel.ObservabilityConfig;

/**
 * The gate of design §4.2 for {@code Linker.Option.critical(true)} on the {@code aster_metric_*}
 * downcalls: {@code OtelNative} keeps them {@code critical(false)} (a record can take the SDK's
 * write lock when it creates a series, and a critical downcall must not block). Compare the hot
 * path with a pre-built attribute set against the ad-hoc key/value path; only the former is a
 * candidate for {@code critical(true)}, and only if this shows a material gain and the steady-state
 * (no new series) latency is bounded.
 *
 * <p>Run with the {@code run-benchmarks} execution of the JMH plugin ({@code
 * site.aster.benchmarks.*}).
 */
@State(Scope.Benchmark)
@BenchmarkMode(Mode.AverageTime)
@OutputTimeUnit(TimeUnit.NANOSECONDS)
@Warmup(iterations = 3, time = 1, timeUnit = TimeUnit.SECONDS)
@Measurement(iterations = 5, time = 1, timeUnit = TimeUnit.SECONDS)
@Fork(1)
public class MetricsBenchmark {

  private Metrics.Counter counter;
  private Metrics.Histogram histogram;
  private Metrics.Attributes attrs;
  private final Map<String, String> adHoc = Map.of("region", "eu");

  @Setup
  public void setup() {
    Observability.shutdown();
    Observability.init(
        ObservabilityConfig.builder().fromEnv(false).metrics(true).logBridge(false).build());
    counter = Metrics.counter("bench.counter", "Benchmark counter", "");
    histogram = Metrics.histogram("bench.histogram", "Benchmark histogram", "s");
    attrs = Metrics.Attributes.of("region", "eu");
  }

  @TearDown
  public void tearDown() {
    Observability.shutdown(Duration.ofSeconds(1));
  }

  @Benchmark
  public void counterNoAttributes() {
    counter.add(1);
  }

  @Benchmark
  public void counterPrebuiltAttributes() {
    counter.add(1, attrs);
  }

  @Benchmark
  public void counterAdHocAttributes() {
    counter.add(1, adHoc);
  }

  @Benchmark
  public void histogramPrebuiltAttributes() {
    histogram.record(0.01, attrs);
  }
}
