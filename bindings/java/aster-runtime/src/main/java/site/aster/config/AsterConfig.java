package site.aster.config;

import java.nio.file.Files;
import java.nio.file.Path;
import java.util.ArrayList;
import java.util.List;
import org.tomlj.Toml;
import org.tomlj.TomlParseResult;
import org.tomlj.TomlTable;

/**
 * Unified configuration for {@code AsterServer} / {@code AsterClient} / {@code IrohNode}.
 *
 * <p>Three-layer resolution (later wins): built-in defaults, an {@code aster.toml} file, then
 * {@code ASTER_*} environment variables, then builder calls.
 *
 * <p>The <b>node knobs</b> (relay, secret key / key file, storage, GC, bind address, hooks,
 * discovery, monitoring, transports, portmapper, proxy, QUIC tuning; table in {@code
 * docs/aster-rust-getstarted.md} §2.2) are resolved by the Rust {@code aster::AsterConfig} through
 * the C ABI ({@code aster_config_from_env} / {@code aster_config_from_toml}), so Java reads env and
 * TOML exactly like Rust, Python and TypeScript. Nodes start through {@code aster_node_start}. The
 * trust, connect and logging fields are still read here.
 *
 * <p>Usage:
 *
 * <pre>{@code
 * // From env only (the common case for containers):
 * AsterConfig config = AsterConfig.fromEnv();
 *
 * // Inline (testing): loopback only, relays off.
 * AsterConfig config = AsterConfig.builder()
 *     .relayMode("disabled")
 *     .bindAddr("127.0.0.1:0")
 *     .allowAllConsumers(true)
 *     .build();
 * }</pre>
 */
public final class AsterConfig {

  // ── Trust ──────────────────────────────────────────────────────────────
  private byte[] rootPubkey;
  private String rootPubkeyFile;
  private String enrollmentCredentialFile;
  private String enrollmentCredentialIid;
  private boolean allowAllConsumers;
  private boolean allowAllProducers = true;

  // ── Connect ────────────────────────────────────────────────────────────
  private String endpointAddr;

  // ── Storage ────────────────────────────────────────────────────────────
  private String storagePath;

  // ── Network (node knobs; see class docs) ───────────────────────────────
  private byte[] secretKey;
  private String secretKeyFile;
  private String relayMode;
  private List<String> relayUrls = List.of();
  private long gcIntervalMs;
  private String hookFailureMode = "fail_open";
  private List<byte[]> alpns = List.of();
  private Long transportMaxConcurrentBidiStreams;
  private Long transportMaxConcurrentUniStreams;
  private Long transportStreamReceiveWindow;
  private Long transportReceiveWindow;
  private Long transportSendWindow;
  private Long transportMaxIdleTimeoutMs;
  private Long transportKeepAliveIntervalMs;
  private Integer transportInitialMtu;
  private Long transportDatagramReceiveBufferSize;
  private Long transportDatagramSendBufferSize;
  private Boolean transportSendFairness;
  private Boolean transportEnableSegmentationOffload;
  private String bindAddr;
  private boolean enableMonitoring;
  private boolean enableHooks;
  private int hookTimeoutMs = 5000;
  private boolean clearIpTransports;
  private boolean clearRelayTransports;
  private String portmapperConfig;
  private String proxyUrl;
  private boolean proxyFromEnv;
  private boolean localDiscovery;

  // ── Logging ────────────────────────────────────────────────────────────
  private String logFormat = "text";
  private String logLevel = "info";
  private boolean logMask = true;

  // ── Ops endpoints (/healthz, /readyz, /metrics; Rust aster::ops) ───────
  private int healthPort;
  private String healthHost = "127.0.0.1";
  private Boolean healthEnabled;

  // ── Identity ───────────────────────────────────────────────────────────
  private String identityFile;

  private AsterConfig() {}

  // ── Getters ────────────────────────────────────────────────────────────

  public byte[] rootPubkey() {
    return rootPubkey;
  }

  public String rootPubkeyFile() {
    return rootPubkeyFile;
  }

  public String enrollmentCredentialFile() {
    return enrollmentCredentialFile;
  }

  public String enrollmentCredentialIid() {
    return enrollmentCredentialIid;
  }

  public boolean allowAllConsumers() {
    return allowAllConsumers;
  }

  public boolean allowAllProducers() {
    return allowAllProducers;
  }

  public String endpointAddr() {
    return endpointAddr;
  }

  public String storagePath() {
    return storagePath;
  }

  public byte[] secretKey() {
    return secretKey;
  }

  public String relayMode() {
    return relayMode;
  }

  public String bindAddr() {
    return bindAddr;
  }

  public boolean enableMonitoring() {
    return enableMonitoring;
  }

  public boolean enableHooks() {
    return enableHooks;
  }

  public int hookTimeoutMs() {
    return hookTimeoutMs;
  }

  public boolean localDiscovery() {
    return localDiscovery;
  }

  public String secretKeyFile() {
    return secretKeyFile;
  }

  /** Relay URLs for {@code relayMode("custom")} (a non-empty list alone also selects custom). */
  public List<String> relayUrls() {
    return relayUrls;
  }

  /** Blob GC interval in ms; 0 = no GC. */
  public long gcIntervalMs() {
    return gcIntervalMs;
  }

  /** {@code "fail_open"} (default) or {@code "fail_closed"}. */
  public String hookFailureMode() {
    return hookFailureMode;
  }

  /** Extra inbound ALPNs the node registers. */
  public List<byte[]> alpns() {
    return alpns;
  }

  public boolean clearIpTransports() {
    return clearIpTransports;
  }

  public boolean clearRelayTransports() {
    return clearRelayTransports;
  }

  /** {@code null} (transport default), {@code "enabled"} or {@code "disabled"}. */
  public String portmapperConfig() {
    return portmapperConfig;
  }

  public String proxyUrl() {
    return proxyUrl;
  }

  public boolean proxyFromEnv() {
    return proxyFromEnv;
  }

  public Long transportMaxConcurrentBidiStreams() {
    return transportMaxConcurrentBidiStreams;
  }

  public Long transportMaxConcurrentUniStreams() {
    return transportMaxConcurrentUniStreams;
  }

  public Long transportStreamReceiveWindow() {
    return transportStreamReceiveWindow;
  }

  public Long transportReceiveWindow() {
    return transportReceiveWindow;
  }

  public Long transportSendWindow() {
    return transportSendWindow;
  }

  public Long transportMaxIdleTimeoutMs() {
    return transportMaxIdleTimeoutMs;
  }

  public Long transportKeepAliveIntervalMs() {
    return transportKeepAliveIntervalMs;
  }

  public Integer transportInitialMtu() {
    return transportInitialMtu;
  }

  public Long transportDatagramReceiveBufferSize() {
    return transportDatagramReceiveBufferSize;
  }

  public Long transportDatagramSendBufferSize() {
    return transportDatagramSendBufferSize;
  }

  public Boolean transportSendFairness() {
    return transportSendFairness;
  }

  public Boolean transportEnableSegmentationOffload() {
    return transportEnableSegmentationOffload;
  }

  public String logFormat() {
    return logFormat;
  }

  public String logLevel() {
    return logLevel;
  }

  public boolean logMask() {
    return logMask;
  }

  public String identityFile() {
    return identityFile;
  }

  /** TCP port of the ops listener; {@code 0} = disabled unless {@link #healthEnabled()}. */
  public int healthPort() {
    return healthPort;
  }

  /** Bind host of the ops listener (localhost only by default; pod probes need 0.0.0.0). */
  public String healthHost() {
    return healthHost;
  }

  /**
   * Forces the ops listener on or off; {@code null} enables it when {@link #healthPort()} is not 0.
   */
  public Boolean healthEnabled() {
    return healthEnabled;
  }

  /** Whether the ops listener should run: {@link #healthEnabled()} when set, else port != 0. */
  public boolean isHealthEnabled() {
    return healthEnabled != null ? healthEnabled : healthPort != 0;
  }

  // ── Factory ────────────────────────────────────────────────────────────

  /**
   * Build config from {@code ASTER_*} environment variables only. Node knobs are resolved by the
   * Rust config ({@code aster_config_from_env}); a malformed value throws.
   */
  public static AsterConfig fromEnv() {
    return site.aster.ffi.NativeConfig.applyEnv(builder()).applyEnv().build();
  }

  /**
   * Build config from a TOML file, with env-var overrides (env wins). Node knobs are resolved by
   * the Rust config ({@code aster_config_from_toml}); a malformed file throws.
   */
  public static AsterConfig fromFile(Path path) {
    return site.aster.ffi.NativeConfig.applyToml(path, builder())
        .applyToml(path)
        .applyEnv()
        .build();
  }

  /** Build config from a TOML file, with env-var overrides (env wins). */
  public static AsterConfig fromFile(String path) {
    return fromFile(Path.of(path));
  }

  public static Builder builder() {
    return new Builder();
  }

  // ── Conversion ─────────────────────────────────────────────────────────

  /** Resolve the root public key from rootPubkey, rootPubkeyFile, or null. */
  public byte[] resolveRootPubkey() {
    if (rootPubkey != null) return rootPubkey;
    if (rootPubkeyFile != null) {
      byte[] loaded = loadPubkeyFromFile(rootPubkeyFile);
      if (loaded != null) {
        rootPubkey = loaded;
        return loaded;
      }
    }
    return null;
  }

  // ── Builder ────────────────────────────────────────────────────────────

  public static final class Builder {
    private final AsterConfig c = new AsterConfig();

    // Trust
    public Builder rootPubkey(byte[] key) {
      c.rootPubkey = key;
      return this;
    }

    public Builder rootPubkeyFile(String path) {
      c.rootPubkeyFile = path;
      return this;
    }

    public Builder enrollmentCredentialFile(String path) {
      c.enrollmentCredentialFile = path;
      return this;
    }

    public Builder enrollmentCredentialIid(String iid) {
      c.enrollmentCredentialIid = iid;
      return this;
    }

    public Builder allowAllConsumers(boolean v) {
      c.allowAllConsumers = v;
      return this;
    }

    public Builder allowAllProducers(boolean v) {
      c.allowAllProducers = v;
      return this;
    }

    // Connect
    public Builder endpointAddr(String addr) {
      c.endpointAddr = addr;
      return this;
    }

    // Storage
    public Builder storagePath(String path) {
      c.storagePath = path;
      return this;
    }

    // Network
    public Builder secretKey(byte[] key) {
      c.secretKey = key;
      return this;
    }

    /**
     * {@code "default"}, {@code "disabled"}, {@code "staging"}, {@code "custom"} (with {@link
     * #relayUrls}) or a bare relay URL (means custom).
     */
    public Builder relayMode(String mode) {
      c.relayMode = mode;
      return this;
    }

    public Builder relayUrls(List<String> urls) {
      c.relayUrls = urls == null ? List.of() : List.copyOf(urls);
      return this;
    }

    /** Secret key file (64 hex chars): loaded if present, else created at node start. */
    public Builder secretKeyFile(String path) {
      c.secretKeyFile = path;
      return this;
    }

    /** Blob GC interval in ms; 0 disables GC. */
    public Builder gcIntervalMs(long ms) {
      c.gcIntervalMs = ms;
      return this;
    }

    /** {@code "fail_open"} or {@code "fail_closed"}: what a hook timeout means. */
    public Builder hookFailureMode(String mode) {
      c.hookFailureMode = mode;
      return this;
    }

    /** Replace the extra inbound ALPN list. */
    public Builder alpns(List<byte[]> alpns) {
      c.alpns = alpns == null ? List.of() : List.copyOf(alpns);
      return this;
    }

    /** Add one extra inbound ALPN. */
    public Builder alpn(byte[] alpn) {
      List<byte[]> next = new ArrayList<>(c.alpns);
      next.add(alpn);
      c.alpns = List.copyOf(next);
      return this;
    }

    /** Relay-only. */
    public Builder clearIpTransports(boolean v) {
      c.clearIpTransports = v;
      return this;
    }

    /** Direct-only. */
    public Builder clearRelayTransports(boolean v) {
      c.clearRelayTransports = v;
      return this;
    }

    /** Enable (default) or disable the UPnP / NAT-PMP / PCP portmapper. */
    public Builder portmapper(boolean enabled) {
      c.portmapperConfig = enabled ? "enabled" : "disabled";
      return this;
    }

    /** {@code null}, {@code "enabled"} or {@code "disabled"}. */
    public Builder portmapperConfig(String value) {
      c.portmapperConfig = value;
      return this;
    }

    public Builder proxyUrl(String url) {
      c.proxyUrl = url;
      return this;
    }

    public Builder proxyFromEnv(boolean v) {
      c.proxyFromEnv = v;
      return this;
    }

    public Builder transportMaxConcurrentBidiStreams(Long n) {
      c.transportMaxConcurrentBidiStreams = n;
      return this;
    }

    public Builder transportMaxConcurrentUniStreams(Long n) {
      c.transportMaxConcurrentUniStreams = n;
      return this;
    }

    public Builder transportStreamReceiveWindow(Long bytes) {
      c.transportStreamReceiveWindow = bytes;
      return this;
    }

    public Builder transportReceiveWindow(Long bytes) {
      c.transportReceiveWindow = bytes;
      return this;
    }

    public Builder transportSendWindow(Long bytes) {
      c.transportSendWindow = bytes;
      return this;
    }

    public Builder transportMaxIdleTimeoutMs(Long ms) {
      c.transportMaxIdleTimeoutMs = ms;
      return this;
    }

    public Builder transportKeepAliveIntervalMs(Long ms) {
      c.transportKeepAliveIntervalMs = ms;
      return this;
    }

    public Builder transportInitialMtu(Integer bytes) {
      c.transportInitialMtu = bytes;
      return this;
    }

    public Builder transportDatagramReceiveBufferSize(Long bytes) {
      c.transportDatagramReceiveBufferSize = bytes;
      return this;
    }

    public Builder transportDatagramSendBufferSize(Long bytes) {
      c.transportDatagramSendBufferSize = bytes;
      return this;
    }

    public Builder transportSendFairness(Boolean v) {
      c.transportSendFairness = v;
      return this;
    }

    public Builder transportEnableSegmentationOffload(Boolean v) {
      c.transportEnableSegmentationOffload = v;
      return this;
    }

    public Builder bindAddr(String addr) {
      c.bindAddr = addr;
      return this;
    }

    public Builder enableMonitoring(boolean v) {
      c.enableMonitoring = v;
      return this;
    }

    public Builder enableHooks(boolean v) {
      c.enableHooks = v;
      return this;
    }

    public Builder hookTimeoutMs(int ms) {
      c.hookTimeoutMs = ms;
      return this;
    }

    public Builder localDiscovery(boolean v) {
      c.localDiscovery = v;
      return this;
    }

    // Logging
    public Builder logFormat(String fmt) {
      c.logFormat = fmt;
      return this;
    }

    public Builder logLevel(String lvl) {
      c.logLevel = lvl;
      return this;
    }

    public Builder logMask(boolean v) {
      c.logMask = v;
      return this;
    }

    // Ops endpoints
    /** Ops listener port ({@code ASTER_HEALTH_PORT}, {@code [health] port}); 0 = off. */
    public Builder healthPort(int port) {
      if (port < 0 || port > 65535) {
        throw new IllegalArgumentException("health port out of range: " + port);
      }
      c.healthPort = port;
      return this;
    }

    /** Ops listener bind host ({@code ASTER_HEALTH_HOST}, {@code [health] host}). */
    public Builder healthHost(String host) {
      c.healthHost = host;
      return this;
    }

    /** Force the ops listener on/off ({@code ASTER_HEALTH_ENABLED}, {@code [health] enabled}). */
    public Builder healthEnabled(Boolean v) {
      c.healthEnabled = v;
      return this;
    }

    // Identity
    public Builder identityFile(String path) {
      c.identityFile = path;
      return this;
    }

    /**
     * Apply the trust / connect / logging values of an {@code aster.toml} file. Call before {@link
     * #applyEnv()} so env wins. Node knobs ({@code [network]}, {@code [storage]}, {@code
     * [identity]}) are read by {@link AsterConfig#fromFile} through the Rust config.
     */
    public Builder applyToml(Path path) {
      try {
        TomlParseResult toml = Toml.parse(path);

        // [trust]
        TomlTable trust = toml.getTable("trust");
        if (trust != null) {
          tomlString(trust, "root_pubkey_file", v -> c.rootPubkeyFile = v);
          tomlString(trust, "enrollment_credential", v -> c.enrollmentCredentialFile = v);
          tomlString(trust, "enrollment_credential_iid", v -> c.enrollmentCredentialIid = v);
          tomlBool(trust, "allow_all_consumers", v -> c.allowAllConsumers = v);
          tomlBool(trust, "allow_all_producers", v -> c.allowAllProducers = v);
          if (trust.contains("root_pubkey")) {
            c.rootPubkey = hexToBytes(trust.getString("root_pubkey"));
          }
        }

        // [connect]
        TomlTable connect = toml.getTable("connect");
        if (connect != null) {
          tomlString(connect, "endpoint_addr", v -> c.endpointAddr = v);
        }

        // [logging]
        TomlTable logging = toml.getTable("logging");
        if (logging != null) {
          tomlString(logging, "format", v -> c.logFormat = v.toLowerCase());
          tomlString(logging, "level", v -> c.logLevel = v.toLowerCase());
          tomlBool(logging, "mask", v -> c.logMask = v);
        }

        // [health]
        TomlTable health = toml.getTable("health");
        if (health != null) {
          if (health.contains("port")) {
            healthPort(Math.toIntExact(health.getLong("port")));
          }
          tomlString(health, "host", v -> c.healthHost = v);
          tomlBool(health, "enabled", v -> c.healthEnabled = v);
        }
      } catch (Exception e) {
        throw new RuntimeException(
            "Failed to parse TOML config: " + path + ": " + e.getMessage(), e);
      }
      return this;
    }

    /**
     * Apply the trust / connect / logging {@code ASTER_*} variables (overrides prior builder
     * calls). Node knobs ({@code ASTER_RELAY_MODE}, {@code ASTER_BIND_ADDR}, ...) are read by
     * {@link #applyNodeEnv()} / {@link AsterConfig#fromEnv} through the Rust config.
     */
    public Builder applyEnv() {
      envString("ASTER_ROOT_PUBKEY_FILE", v -> c.rootPubkeyFile = v);
      envString("ASTER_ENROLLMENT_CREDENTIAL", v -> c.enrollmentCredentialFile = v);
      envString("ASTER_ENROLLMENT_CREDENTIAL_IID", v -> c.enrollmentCredentialIid = v);
      envString("ASTER_ENDPOINT_ADDR", v -> c.endpointAddr = v);
      envString("ASTER_LOG_FORMAT", v -> c.logFormat = v.toLowerCase());
      envString("ASTER_LOG_LEVEL", v -> c.logLevel = v.toLowerCase());

      envBool("ASTER_ALLOW_ALL_CONSUMERS", v -> c.allowAllConsumers = v);
      envBool("ASTER_ALLOW_ALL_PRODUCERS", v -> c.allowAllProducers = v);
      envBool("ASTER_LOG_MASK", v -> c.logMask = v);

      envString(
          "ASTER_HEALTH_PORT",
          v -> {
            try {
              healthPort(Integer.parseInt(v));
            } catch (NumberFormatException e) {
              throw new IllegalArgumentException(
                  "ASTER_HEALTH_PORT: expected a port number, got \"" + v + "\"", e);
            }
          });
      envString("ASTER_HEALTH_HOST", v -> c.healthHost = v);
      envBool("ASTER_HEALTH_ENABLED", v -> c.healthEnabled = v);

      envBytes("ASTER_ROOT_PUBKEY", v -> c.rootPubkey = v);

      return this;
    }

    /**
     * Replace every node knob with the values the Rust config resolves from the {@code ASTER_*}
     * environment ({@code aster_config_from_env}; unset variables give the defaults).
     */
    public Builder applyNodeEnv() {
      return site.aster.ffi.NativeConfig.applyEnv(this);
    }

    public AsterConfig build() {
      return c;
    }

    // ── Env helpers ────────────────────────────────────────────────────

    private static void envString(String key, java.util.function.Consumer<String> setter) {
      String v = System.getenv(key);
      if (v != null) setter.accept(v.trim());
    }

    private static void envBool(String key, java.util.function.Consumer<Boolean> setter) {
      String v = System.getenv(key);
      if (v == null) return;
      v = v.trim().toLowerCase();
      if (List.of("true", "1", "yes", "on").contains(v)) setter.accept(true);
      else if (List.of("false", "0", "no", "off").contains(v)) setter.accept(false);
    }

    private static void envBytes(String key, java.util.function.Consumer<byte[]> setter) {
      String v = System.getenv(key);
      if (v != null && !v.isBlank()) {
        setter.accept(hexToBytes(v.trim()));
      }
    }

    // TOML helpers
    private static void tomlString(
        TomlTable t, String key, java.util.function.Consumer<String> setter) {
      if (t.contains(key)) setter.accept(t.getString(key));
    }

    private static void tomlBool(
        TomlTable t, String key, java.util.function.Consumer<Boolean> setter) {
      if (t.contains(key)) setter.accept(t.getBoolean(key));
    }
  }

  // ── Helpers ──────────────────────────────────────────────────────────

  private static byte[] hexToBytes(String hex) {
    int len = hex.length();
    byte[] data = new byte[len / 2];
    for (int i = 0; i < len; i += 2) {
      data[i / 2] =
          (byte)
              ((Character.digit(hex.charAt(i), 16) << 4) + Character.digit(hex.charAt(i + 1), 16));
    }
    return data;
  }

  private static byte[] loadPubkeyFromFile(String path) {
    try {
      String content = Files.readString(Path.of(path)).trim();
      if (content.startsWith("{")) {
        // JSON: extract "public_key" field (minimal parsing)
        int idx = content.indexOf("\"public_key\"");
        if (idx >= 0) {
          int colon = content.indexOf(':', idx);
          int quote1 = content.indexOf('"', colon + 1);
          int quote2 = content.indexOf('"', quote1 + 1);
          if (quote1 >= 0 && quote2 > quote1) {
            return hexToBytes(content.substring(quote1 + 1, quote2));
          }
        }
      }
      // Plain hex
      if (content.length() == 64) {
        return hexToBytes(content);
      }
    } catch (Exception ignored) {
    }
    return null;
  }
}
