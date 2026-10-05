package site.aster.otel;

import java.util.LinkedHashMap;
import java.util.Map;

/**
 * Configuration of the observability pipeline ({@code aster_observability_config_t}). Precedence,
 * highest first: these fields, then {@code OTEL_*} / {@code ASTER_*} environment variables (when
 * {@link Builder#fromEnv} is on), then the {@code [observability]} TOML table ({@link
 * Builder#configToml}), then defaults.
 *
 * <p>Signals: {@code true} exports that signal over OTLP. {@code false} and {@code null} both mean
 * "not forced": with {@code fromEnv} the environment decides, without it the signal is off.
 *
 * @param serviceName {@code service.name} resource attribute, or {@code null}
 * @param otlpEndpoint OTLP/HTTP collector base URL, or {@code null}
 * @param otlpHeaders request headers (often a bearer token); never logged
 * @param resourceAttributes extra resource attributes
 * @param traces export traces over OTLP
 * @param metrics export metrics over OTLP
 * @param otlpLogs export Rust logs over OTLP
 * @param sampleRatio parent-based trace-id ratio sampler in [0, 1], or {@code null} for the default
 * @param logFilter {@code EnvFilter} directives for Rust events (e.g. {@code warn,aster=debug}).
 *     Pins the Rust filter; {@code null} follows the {@code aster} SLF4J logger's level
 * @param logQueue bound of the Rust log queue (0 = the default, 8192 records)
 * @param logBridge deliver Rust log records to SLF4J (default true)
 * @param fromEnv apply {@code OTEL_*} / {@code ASTER_*} under the explicit fields
 * @param configToml the {@code [observability]} TOML table (header optional); the only place for
 *     {@code attribute_extractors} and {@code propagate_as_baggage}
 */
public record ObservabilityConfig(
    String serviceName,
    String otlpEndpoint,
    Map<String, String> otlpHeaders,
    Map<String, String> resourceAttributes,
    Boolean traces,
    Boolean metrics,
    Boolean otlpLogs,
    Double sampleRatio,
    String logFilter,
    int logQueue,
    boolean logBridge,
    boolean fromEnv,
    String configToml) {

  public ObservabilityConfig {
    otlpHeaders = Map.copyOf(otlpHeaders);
    resourceAttributes = Map.copyOf(resourceAttributes);
  }

  public static Builder builder() {
    return new Builder();
  }

  /** Builder with the defaults: log bridge on, environment applied. */
  public static final class Builder {
    private String serviceName;
    private String otlpEndpoint;
    private final Map<String, String> otlpHeaders = new LinkedHashMap<>();
    private final Map<String, String> resourceAttributes = new LinkedHashMap<>();
    private Boolean traces;
    private Boolean metrics;
    private Boolean otlpLogs;
    private Double sampleRatio;
    private String logFilter;
    private int logQueue;
    private boolean logBridge = true;
    private boolean fromEnv = true;
    private String configToml;

    private Builder() {}

    public Builder serviceName(String v) {
      this.serviceName = v;
      return this;
    }

    public Builder otlpEndpoint(String v) {
      this.otlpEndpoint = v;
      return this;
    }

    public Builder otlpHeader(String name, String value) {
      this.otlpHeaders.put(name, value);
      return this;
    }

    public Builder resourceAttribute(String name, String value) {
      this.resourceAttributes.put(name, value);
      return this;
    }

    public Builder traces(Boolean v) {
      this.traces = v;
      return this;
    }

    public Builder metrics(Boolean v) {
      this.metrics = v;
      return this;
    }

    public Builder otlpLogs(Boolean v) {
      this.otlpLogs = v;
      return this;
    }

    public Builder sampleRatio(Double v) {
      this.sampleRatio = v;
      return this;
    }

    public Builder logFilter(String v) {
      this.logFilter = v;
      return this;
    }

    public Builder logQueue(int v) {
      this.logQueue = v;
      return this;
    }

    public Builder logBridge(boolean v) {
      this.logBridge = v;
      return this;
    }

    public Builder fromEnv(boolean v) {
      this.fromEnv = v;
      return this;
    }

    public Builder configToml(String v) {
      this.configToml = v;
      return this;
    }

    public ObservabilityConfig build() {
      return new ObservabilityConfig(
          serviceName,
          otlpEndpoint,
          otlpHeaders,
          resourceAttributes,
          traces,
          metrics,
          otlpLogs,
          sampleRatio,
          logFilter,
          logQueue,
          logBridge,
          fromEnv,
          configToml);
    }
  }
}
