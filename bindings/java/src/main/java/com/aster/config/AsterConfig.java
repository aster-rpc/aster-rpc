package com.aster.config;

import java.nio.file.Files;
import java.nio.file.Path;
import java.util.Base64;
import java.util.List;
import org.tomlj.Toml;
import org.tomlj.TomlParseResult;
import org.tomlj.TomlTable;

/**
 * Unified configuration for AsterServer.
 *
 * <p>Three-layer resolution (later wins):
 *
 * <ol>
 *   <li>Built-in defaults (ephemeral key, in-memory store, all gates open).
 *   <li>TOML config file ({@code aster.toml}) -- requires a TOML library (future).
 *   <li>{@code ASTER_*} environment variables.
 * </ol>
 *
 * <p>Usage:
 *
 * <pre>{@code
 * // From env only (the common case for containers):
 * AsterConfig config = AsterConfig.fromEnv();
 *
 * // Inline (testing):
 * AsterConfig config = AsterConfig.builder()
 *     .allowAllConsumers(true)
 *     .storagePath("/var/lib/aster")
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

  // ── Network ────────────────────────────────────────────────────────────
  private byte[] secretKey;
  private String relayMode;
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

  // ── Factory ────────────────────────────────────────────────────────────

  /** Build config from {@code ASTER_*} environment variables only. */
  public static AsterConfig fromEnv() {
    return builder().applyEnv().build();
  }

  /** Build config from a TOML file, with env-var overrides (env wins). */
  public static AsterConfig fromFile(Path path) {
    return builder().applyToml(path).applyEnv().build();
  }

  /** Build config from a TOML file, with env-var overrides (env wins). */
  public static AsterConfig fromFile(String path) {
    return fromFile(Path.of(path));
  }

  public static Builder builder() {
    return new Builder();
  }

  // ── Conversion ─────────────────────────────────────────────────────────

  /** Convert network fields to an {@link EndpointConfig} for the FFI layer. */
  public EndpointConfig toEndpointConfig() {
    EndpointConfig ec = new EndpointConfig();
    if (secretKey != null) ec.secretKey(secretKey);
    if (relayMode != null) ec.relayMode(relayMode.equals("disabled") ? 0 : 1);
    if (bindAddr != null) ec.bindAddr(bindAddr);
    ec.enableDiscovery(localDiscovery);
    ec.enableHooks(enableHooks);
    ec.hookTimeoutMs(hookTimeoutMs);
    ec.clearIpTransports(clearIpTransports);
    ec.clearRelayTransports(clearRelayTransports);
    if (proxyUrl != null) ec.proxyUrl(proxyUrl);
    ec.proxyFromEnv(proxyFromEnv);
    return ec;
  }

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

    public Builder relayMode(String mode) {
      c.relayMode = mode;
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

    // Identity
    public Builder identityFile(String path) {
      c.identityFile = path;
      return this;
    }

    /**
     * Apply values from an {@code aster.toml} file. Call before {@link #applyEnv()} so env wins.
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

        // [storage]
        TomlTable storage = toml.getTable("storage");
        if (storage != null) {
          tomlString(storage, "path", v -> c.storagePath = v);
        }

        // [network]
        TomlTable network = toml.getTable("network");
        if (network != null) {
          tomlString(network, "relay_mode", v -> c.relayMode = v);
          tomlString(network, "bind_addr", v -> c.bindAddr = v);
          tomlString(network, "portmapper_config", v -> c.portmapperConfig = v);
          tomlString(network, "proxy_url", v -> c.proxyUrl = v);
          tomlBool(network, "enable_monitoring", v -> c.enableMonitoring = v);
          tomlBool(network, "enable_hooks", v -> c.enableHooks = v);
          tomlBool(network, "clear_ip_transports", v -> c.clearIpTransports = v);
          tomlBool(network, "clear_relay_transports", v -> c.clearRelayTransports = v);
          tomlBool(network, "proxy_from_env", v -> c.proxyFromEnv = v);
          tomlBool(network, "local_discovery", v -> c.localDiscovery = v);
          if (network.contains("hook_timeout_ms")) {
            c.hookTimeoutMs = network.getLong("hook_timeout_ms").intValue();
          }
          if (network.contains("secret_key")) {
            c.secretKey = Base64.getDecoder().decode(network.getString("secret_key"));
          }
        }

        // [logging]
        TomlTable logging = toml.getTable("logging");
        if (logging != null) {
          tomlString(logging, "format", v -> c.logFormat = v.toLowerCase());
          tomlString(logging, "level", v -> c.logLevel = v.toLowerCase());
          tomlBool(logging, "mask", v -> c.logMask = v);
        }
      } catch (Exception e) {
        throw new RuntimeException(
            "Failed to parse TOML config: " + path + ": " + e.getMessage(), e);
      }
      return this;
    }

    /** Apply {@code ASTER_*} environment variables (overrides any prior builder calls). */
    public Builder applyEnv() {
      envString("ASTER_ROOT_PUBKEY_FILE", v -> c.rootPubkeyFile = v);
      envString("ASTER_ENROLLMENT_CREDENTIAL", v -> c.enrollmentCredentialFile = v);
      envString("ASTER_ENROLLMENT_CREDENTIAL_IID", v -> c.enrollmentCredentialIid = v);
      envString("ASTER_ENDPOINT_ADDR", v -> c.endpointAddr = v);
      envString("ASTER_STORAGE_PATH", v -> c.storagePath = v.isEmpty() ? null : v);
      envString("ASTER_RELAY_MODE", v -> c.relayMode = v.isEmpty() ? null : v);
      envString("ASTER_BIND_ADDR", v -> c.bindAddr = v.isEmpty() ? null : v);
      envString("ASTER_PORTMAPPER_CONFIG", v -> c.portmapperConfig = v.isEmpty() ? null : v);
      envString("ASTER_PROXY_URL", v -> c.proxyUrl = v.isEmpty() ? null : v);
      envString("ASTER_IDENTITY_FILE", v -> c.identityFile = v);
      envString("ASTER_LOG_FORMAT", v -> c.logFormat = v.toLowerCase());
      envString("ASTER_LOG_LEVEL", v -> c.logLevel = v.toLowerCase());

      envBool("ASTER_ALLOW_ALL_CONSUMERS", v -> c.allowAllConsumers = v);
      envBool("ASTER_ALLOW_ALL_PRODUCERS", v -> c.allowAllProducers = v);
      envBool("ASTER_ENABLE_MONITORING", v -> c.enableMonitoring = v);
      envBool("ASTER_ENABLE_HOOKS", v -> c.enableHooks = v);
      envBool("ASTER_CLEAR_IP_TRANSPORTS", v -> c.clearIpTransports = v);
      envBool("ASTER_CLEAR_RELAY_TRANSPORTS", v -> c.clearRelayTransports = v);
      envBool("ASTER_PROXY_FROM_ENV", v -> c.proxyFromEnv = v);
      envBool("ASTER_LOCAL_DISCOVERY", v -> c.localDiscovery = v);
      envBool("ASTER_LOG_MASK", v -> c.logMask = v);

      envInt("ASTER_HOOK_TIMEOUT_MS", v -> c.hookTimeoutMs = v);

      envBytes("ASTER_ROOT_PUBKEY", v -> c.rootPubkey = v);
      envBase64("ASTER_SECRET_KEY", v -> c.secretKey = v);

      return this;
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

    private static void envInt(String key, java.util.function.Consumer<Integer> setter) {
      String v = System.getenv(key);
      if (v != null) {
        try {
          setter.accept(Integer.parseInt(v.trim()));
        } catch (NumberFormatException ignored) {
        }
      }
    }

    private static void envBytes(String key, java.util.function.Consumer<byte[]> setter) {
      String v = System.getenv(key);
      if (v != null && !v.isBlank()) {
        setter.accept(hexToBytes(v.trim()));
      }
    }

    private static void envBase64(String key, java.util.function.Consumer<byte[]> setter) {
      String v = System.getenv(key);
      if (v != null && !v.isBlank()) {
        setter.accept(Base64.getDecoder().decode(v.trim()));
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
