package site.aster.config;

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
 * TOML exactly like Rust, Python and TypeScript. Nodes start through {@code aster_node_start}.
 *
 * <p>The <b>trust and Enrollment</b> fields ({@code [trust]}, {@code [enrollment]}; design §15.3 of
 * {@code docs/_internal/working_ideas/aster-enrollment.md}) are resolved by the same Rust loader
 * ({@code aster_config_enrollment_from_env} / {@code _from_toml}): same names, defaults and
 * precedence as the Rust SDK. With enrollment configured ({@link #isEnrollmentEnabled()}), {@code
 * AsterServer} admits only nodes holding a credential from the configured root and turns each
 * peer's verified claims into the roles {@code @Requires} checks, on every call; {@code
 * AsterClient} presents its credential, enrolls with each server and renews. A node is open exactly
 * when enrollment is not configured. The connect, logging and health fields are read here.
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
 *     .build();
 * }</pre>
 */
public final class AsterConfig {

  // ── Trust and Enrollment ([trust], [enrollment]) ───────────────────────
  private byte[] rootPubkey;
  private String rootPubkeyFile;
  private String nextRootCommitment;
  private long rootSeq;
  private List<String> rootRotations = List.of();
  private String enrollmentCredentialFile;
  private String enrollmentCredentialChain;
  private site.aster.enrollment.EnrollmentConfig enrollment =
      site.aster.enrollment.EnrollmentConfig.DEFAULTS;

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
  private String hookFailureMode = "fail_closed";
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
  private Boolean reuseLastPort;

  // ── Cluster network settings (C ABI 7.3, [network]) ────────────────────
  private String bindAddrV6;
  private List<String> advertiseAddrs = List.of();
  private List<String> relayCaCerts = List.of();
  private boolean relayCaOnly;
  private String discovery = DISCOVERY_DEFAULT;
  private String discoveryDnsOrigin;
  private List<String> discoveryPkarrRelays = List.of();

  // ── Metrics (C ABI 7.4, [metrics]) ─────────────────────────────────────
  private boolean metricsPeerLabels;
  private int metricsPeerLabelCap = METRICS_PEER_LABEL_CAP_DEFAULT;

  // ── Blob transport ([storage] blob_zstd_level) ─────────────────────────
  private Integer blobZstdLevel;

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

  /** {@link #discovery()}: n0 pkarr and DNS at {@code iroh.link} (custom servers select custom). */
  public static final String DISCOVERY_DEFAULT = "default";

  /** {@link #discovery()}: no address lookup; dial peers by full address. */
  public static final String DISCOVERY_NONE = "none";

  /** {@link #discovery()}: local-network mDNS only. */
  public static final String DISCOVERY_LOCAL = "local";

  /** {@link #discovery()}: only {@link #discoveryDnsOrigin()} / {@link #discoveryPkarrRelays()}. */
  public static final String DISCOVERY_CUSTOM = "custom";

  /** {@code ASTER_METRICS_PEER_LABEL_CAP_DEFAULT}: what {@link #metricsPeerLabelCap()} 0 means. */
  public static final int METRICS_PEER_LABEL_CAP_DEFAULT = 64;

  private AsterConfig() {}

  // ── Getters ────────────────────────────────────────────────────────────

  /**
   * The trusted root (32 bytes; {@code ASTER_ROOT_PUBKEY}, {@code [trust] root_pubkey}), shared
   * with coordination. Required by enrollment.
   */
  public byte[] rootPubkey() {
    return rootPubkey;
  }

  /**
   * File holding the root key (64 hex or {@code {"public_key": …}}; {@code
   * ASTER_ROOT_PUBKEY_FILE}). A configured file that does not exist is an error.
   */
  public String rootPubkeyFile() {
    return rootPubkeyFile;
  }

  /**
   * Root rotation pin (64 hex; {@code ASTER_NEXT_ROOT_COMMITMENT}). Without it every rotation
   * statement is refused.
   */
  public String nextRootCommitment() {
    return nextRootCommitment;
  }

  /** Rotations applied to reach the root ({@code ASTER_ROOT_SEQ}). */
  public long rootSeq() {
    return rootSeq;
  }

  /** Applied rotation statements, oldest first ({@code ASTER_ROOT_ROTATIONS}, comma-separated). */
  public List<String> rootRotations() {
    return rootRotations;
  }

  /**
   * This node's enrollment credential chain file ({@code aster enroll node --out}; {@code
   * ASTER_ENROLLMENT_CREDENTIAL}, {@code [trust] enrollment_credential}). Setting it enables
   * enrollment.
   */
  public String enrollmentCredentialFile() {
    return enrollmentCredentialFile;
  }

  /**
   * The credential chain inline ({@code ASTER_ENROLLMENT_CREDENTIAL_CHAIN}, {@code [trust]
   * enrollment_credential_chain}); wins over the file.
   */
  public String enrollmentCredentialChain() {
    return enrollmentCredentialChain;
  }

  /** The {@code [enrollment]} section ({@code ASTER_ENROLLMENT_*}). */
  public site.aster.enrollment.EnrollmentConfig enrollment() {
    return enrollment;
  }

  /**
   * Whether {@code AsterServer} / {@code AsterClient} install Enrollment from this config: {@code
   * enrollment().enabled()} when set, else whether an audience, fleet namespace, issuer delegation
   * or credential (file or chain) is configured. The root and rotation pin alone do not enable it
   * (Rust {@code AsterConfig::is_enrollment_enabled}).
   */
  public boolean isEnrollmentEnabled() {
    if (enrollment.enabled() != null) {
      return enrollment.enabled();
    }
    return present(enrollment.audience())
        || present(enrollment.fleetNamespace())
        || present(enrollment.issuerDelegationFile())
        || present(enrollmentCredentialFile)
        || present(enrollmentCredentialChain);
  }

  private static boolean present(String s) {
    return s != null && !s.isBlank();
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

  /**
   * {@code "fail_closed"} (default: a stalled or missing admission loop rejects) or the explicit
   * opt-in {@code "fail_open"} (accepts).
   */
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

  /**
   * zstd level for blob transport (Rust {@code AsterConfig::blob_zstd_level}): {@code null} = the
   * default (3), {@code 0} = off (the node serves and fetches blobs on the plain ALPN only), 1..19
   * = that level. Env {@code ASTER_BLOB_ZSTD_LEVEL}; TOML {@code [storage] blob_zstd_level}.
   */
  public Integer blobZstdLevel() {
    return blobZstdLevel;
  }

  /**
   * Re-bind a persistent node's last UDP ports on restart (Rust {@code
   * AsterConfig::reuse_last_port}): {@code null} = the default (on). Env {@code
   * ASTER_REUSE_LAST_PORT}; TOML {@code [network] reuse_last_port}.
   */
  public Boolean reuseLastPort() {
    return reuseLastPort;
  }

  // ── Cluster network settings (C ABI 7.3) ───────────────────────────────

  /**
   * IPv6 bind address, e.g. {@code [::]:7842}: pins the IPv6 UDP port as {@link #bindAddr()} pins
   * the IPv4 one. Env {@code ASTER_BIND_ADDR_V6}; TOML {@code [network] bind_addr_v6}.
   */
  public String bindAddrV6() {
    return bindAddrV6;
  }

  /**
   * Extra direct addresses ({@code IP:port}) to advertise, e.g. a NodePort. Hints only: peers still
   * verify the node key. Env {@code ASTER_ADVERTISE_ADDRS} (comma-separated); TOML {@code [network]
   * advertise_addrs}.
   */
  public List<String> advertiseAddrs() {
    return advertiseAddrs;
  }

  /**
   * PEM files with CA certificates trusted for relay TLS, in addition to the WebPKI roots. Env
   * {@code ASTER_RELAY_CA_CERTS}; TOML {@code [network] relay_ca_certs}.
   */
  public List<String> relayCaCerts() {
    return relayCaCerts;
  }

  /**
   * Trust only {@link #relayCaCerts()} for relay TLS, not the WebPKI roots. Env {@code
   * ASTER_RELAY_CA_ONLY}; TOML {@code [network] relay_ca_only}.
   */
  public boolean relayCaOnly() {
    return relayCaOnly;
  }

  /**
   * How the node publishes its addresses and finds peers by bare node id: {@link
   * #DISCOVERY_DEFAULT}, {@link #DISCOVERY_NONE}, {@link #DISCOVERY_LOCAL} or {@link
   * #DISCOVERY_CUSTOM}. Env {@code ASTER_DISCOVERY}; TOML {@code [network] discovery}.
   */
  public String discovery() {
    return discovery;
  }

  /**
   * DNS origin for custom discovery, e.g. {@code dns.example.internal}. Env {@code
   * ASTER_DISCOVERY_DNS_ORIGIN}; TOML {@code [network] discovery_dns_origin}.
   */
  public String discoveryDnsOrigin() {
    return discoveryDnsOrigin;
  }

  /**
   * Pkarr relay URLs for custom discovery. Env {@code ASTER_DISCOVERY_PKARR_RELAYS}; TOML {@code
   * [network] discovery_pkarr_relays}.
   */
  public List<String> discoveryPkarrRelays() {
    return discoveryPkarrRelays;
  }

  /** Whether any cluster network setting differs from its default. */
  public boolean hasNetworkSettings() {
    String d = discovery == null ? "" : discovery.trim().toLowerCase(java.util.Locale.ROOT);
    return (bindAddrV6 != null && !bindAddrV6.isEmpty())
        || !advertiseAddrs.isEmpty()
        || !relayCaCerts.isEmpty()
        || relayCaOnly
        || (!d.isEmpty() && !d.equals(DISCOVERY_DEFAULT))
        || (discoveryDnsOrigin != null && !discoveryDnsOrigin.isEmpty())
        || !discoveryPkarrRelays.isEmpty();
  }

  // ── Metrics (C ABI 7.4) ────────────────────────────────────────────────

  /**
   * Label {@code aster.peer.bytes} per peer ({@code aster.peer}) instead of by locality level: the
   * first {@link #metricsPeerLabelCap()} peers seen get their own series, later ones count under
   * {@code _OTHER}. Env {@code ASTER_METRICS_PEER_LABELS}; TOML {@code [metrics] peer_labels}.
   */
  public boolean metricsPeerLabels() {
    return metricsPeerLabels;
  }

  /**
   * How many peers get their own {@code aster.peer} series (default {@value
   * #METRICS_PEER_LABEL_CAP_DEFAULT}). Env {@code ASTER_METRICS_PEER_LABEL_CAP}; TOML {@code
   * [metrics] peer_label_cap}.
   */
  public int metricsPeerLabelCap() {
    return metricsPeerLabelCap;
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
    return site.aster.ffi.NativeConfig.applyEnrollmentEnv(
            site.aster.ffi.NativeConfig.applyEnv(builder()).applyEnv())
        .build();
  }

  /**
   * Build config from a TOML file, with env-var overrides (env wins). Node knobs are resolved by
   * the Rust config ({@code aster_config_from_toml}); a malformed file throws.
   */
  public static AsterConfig fromFile(Path path) {
    return site.aster.ffi.NativeConfig.applyEnrollmentToml(
            path, site.aster.ffi.NativeConfig.applyToml(path, builder()).applyToml(path).applyEnv())
        .build();
  }

  /** Build config from a TOML file, with env-var overrides (env wins). */
  public static AsterConfig fromFile(String path) {
    return fromFile(Path.of(path));
  }

  public static Builder builder() {
    return new Builder();
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

    /** Root rotation pin: commitment (64 hex), applied count and statements (oldest first). */
    public Builder rootRotation(String nextCommitment, long seq, List<String> statements) {
      c.nextRootCommitment = nextCommitment;
      c.rootSeq = seq;
      c.rootRotations = statements == null ? List.of() : List.copyOf(statements);
      return this;
    }

    /** This node's credential chain file ({@code aster enroll node --out}). */
    public Builder enrollmentCredentialFile(String path) {
      c.enrollmentCredentialFile = path;
      return this;
    }

    /** This node's credential chain inline (wins over the file). */
    public Builder enrollmentCredentialChain(String chain) {
      c.enrollmentCredentialChain = chain;
      return this;
    }

    /** The {@code [enrollment]} section. */
    public Builder enrollment(site.aster.enrollment.EnrollmentConfig enrollment) {
      c.enrollment =
          enrollment == null ? site.aster.enrollment.EnrollmentConfig.DEFAULTS : enrollment;
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

    /**
     * {@code "fail_closed"} (default) or {@code "fail_open"}: what a hook timeout means. Any value
     * other than {@code "fail_open"} fails closed.
     */
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

    /**
     * zstd level for blob transport: {@code null} = default (3), {@code 0} = off, 1..19 = that
     * level (larger values are clamped to 19). See {@link AsterConfig#blobZstdLevel()}.
     */
    public Builder blobZstdLevel(Integer level) {
      if (level != null && level < 0) {
        throw new IllegalArgumentException(
            "blobZstdLevel must be 0 (off) or a zstd level, got " + level);
      }
      c.blobZstdLevel = level == null ? null : Math.min(level, 19);
      return this;
    }

    /**
     * Re-bind a persistent node's last ports on restart; {@code null} = default (on). See {@link
     * AsterConfig#reuseLastPort()}.
     */
    public Builder reuseLastPort(Boolean v) {
      c.reuseLastPort = v;
      return this;
    }

    /** See {@link AsterConfig#bindAddrV6()}. */
    public Builder bindAddrV6(String addr) {
      c.bindAddrV6 = addr;
      return this;
    }

    /** See {@link AsterConfig#advertiseAddrs()}. */
    public Builder advertiseAddrs(List<String> addrs) {
      c.advertiseAddrs = addrs == null ? List.of() : List.copyOf(addrs);
      return this;
    }

    /** See {@link AsterConfig#relayCaCerts()}. */
    public Builder relayCaCerts(List<String> pemPaths) {
      c.relayCaCerts = pemPaths == null ? List.of() : List.copyOf(pemPaths);
      return this;
    }

    /** See {@link AsterConfig#relayCaOnly()}. */
    public Builder relayCaOnly(boolean v) {
      c.relayCaOnly = v;
      return this;
    }

    /**
     * See {@link AsterConfig#discovery()}; {@code null} = default.
     *
     * @throws IllegalArgumentException for a value other than default / none / local / custom
     */
    public Builder discovery(String mode) {
      String m = mode == null ? "" : mode.trim().toLowerCase(java.util.Locale.ROOT);
      switch (m) {
        case "", DISCOVERY_DEFAULT -> c.discovery = DISCOVERY_DEFAULT;
        case DISCOVERY_NONE, DISCOVERY_LOCAL, DISCOVERY_CUSTOM -> c.discovery = m;
        default ->
            throw new IllegalArgumentException(
                "discovery " + mode + ": want default, none, local or custom");
      }
      return this;
    }

    /** See {@link AsterConfig#discoveryDnsOrigin()}. */
    public Builder discoveryDnsOrigin(String origin) {
      c.discoveryDnsOrigin = origin;
      return this;
    }

    /** See {@link AsterConfig#discoveryPkarrRelays()}. */
    public Builder discoveryPkarrRelays(List<String> urls) {
      c.discoveryPkarrRelays = urls == null ? List.of() : List.copyOf(urls);
      return this;
    }

    /** See {@link AsterConfig#metricsPeerLabels()}. */
    public Builder metricsPeerLabels(boolean v) {
      c.metricsPeerLabels = v;
      return this;
    }

    /** See {@link AsterConfig#metricsPeerLabelCap()}; {@code 0} = the default. */
    public Builder metricsPeerLabelCap(int cap) {
      if (cap < 0) {
        throw new IllegalArgumentException("metricsPeerLabelCap must be nonnegative");
      }
      c.metricsPeerLabelCap = cap == 0 ? METRICS_PEER_LABEL_CAP_DEFAULT : cap;
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
     * Apply the connect / logging / health values of an {@code aster.toml} file. Call before {@link
     * #applyEnv()} so env wins. Node knobs ({@code [network]}, {@code [storage]}, {@code
     * [identity]}) and {@code [trust]} / {@code [enrollment]} are read by {@link
     * AsterConfig#fromFile} through the Rust config.
     */
    public Builder applyToml(Path path) {
      try {
        TomlParseResult toml = Toml.parse(path);

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
     * Apply the connect / logging / health {@code ASTER_*} variables (overrides prior builder
     * calls). Node knobs ({@code ASTER_RELAY_MODE}, {@code ASTER_BIND_ADDR}, ...) and the trust /
     * enrollment variables are read by {@link AsterConfig#fromEnv} through the Rust config.
     */
    public Builder applyEnv() {
      envString("ASTER_ENDPOINT_ADDR", v -> c.endpointAddr = v);
      envString("ASTER_LOG_FORMAT", v -> c.logFormat = v.toLowerCase());
      envString("ASTER_LOG_LEVEL", v -> c.logLevel = v.toLowerCase());

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
}
