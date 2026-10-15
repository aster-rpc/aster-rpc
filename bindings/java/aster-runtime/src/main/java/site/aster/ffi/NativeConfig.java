package site.aster.ffi;

import java.lang.foreign.Arena;
import java.lang.foreign.MemorySegment;
import java.lang.foreign.ValueLayout;
import java.nio.charset.StandardCharsets;
import java.nio.file.Path;
import java.util.ArrayList;
import java.util.List;
import java.util.concurrent.ExecutionException;
import java.util.concurrent.TimeUnit;
import java.util.concurrent.TimeoutException;
import site.aster.config.AsterConfig;
import site.aster.event.IrohEvent;
import site.aster.handle.IrohRuntime;

/**
 * Marshals {@link AsterConfig} to and from the C ABI {@code aster_config_t} (ABI 3.2) and starts
 * nodes with {@code aster_node_start}. Env / TOML loading of the node knobs is done by the Rust
 * {@code aster::AsterConfig} ({@code aster_config_from_env} / {@code aster_config_from_toml}), so
 * Java resolves them exactly like the Rust SDK and the other bindings.
 */
public final class NativeConfig {

  /** {@code sizeof(aster_config_t)}. */
  public static final long SIZE = 288;

  /** {@code aster_enrollment_side_t}: a server (durable state; may run an issuer). */
  public static final int ENROLLMENT_SIDE_SERVER = 0;

  /** {@code aster_enrollment_side_t}: a client (enrolls with each server it calls). */
  public static final int ENROLLMENT_SIDE_CLIENT = 1;

  // aster_relay_mode_t
  static final int RELAY_DEFAULT = 0;
  static final int RELAY_CUSTOM = 1;
  static final int RELAY_DISABLED = 2;
  static final int RELAY_STAGING = 3;

  // ASTER_TRANSPORT_* bits of transport_set
  static final int T_BIDI = 1;
  static final int T_UNI = 1 << 1;
  static final int T_STREAM_RECV_WINDOW = 1 << 2;
  static final int T_RECV_WINDOW = 1 << 3;
  static final int T_SEND_WINDOW = 1 << 4;
  static final int T_IDLE = 1 << 5;
  static final int T_KEEP_ALIVE = 1 << 6;
  static final int T_MTU = 1 << 7;
  static final int T_DGRAM_RECV = 1 << 8;
  static final int T_DGRAM_SEND = 1 << 9;
  static final int T_FAIRNESS = 1 << 10;
  static final int T_GSO = 1 << 11;

  // Field offsets (ffi/src/layout.rs).
  private static final long RELAY_MODE = 4;
  private static final long RELAY_URLS = 8;
  private static final long SECRET_KEY = 24;
  private static final long SECRET_KEY_FILE = 40;
  private static final long STORAGE_PATH = 56;
  private static final long GC_INTERVAL_MS = 72;
  private static final long BIND_ADDR = 80;
  private static final long ALPNS = 96;
  private static final long IDENTITY_FILE = 112;
  private static final long PROXY_URL = 128;
  private static final long HOOK_TIMEOUT_MS = 144;
  private static final long HOOK_FAILURE_MODE = 152;
  private static final long ENABLE_HOOKS = 156;
  private static final long ENABLE_MONITORING = 160;
  private static final long LOCAL_DISCOVERY = 164;
  private static final long CLEAR_IP = 168;
  private static final long CLEAR_RELAY = 172;
  private static final long PORTMAPPER = 176;
  private static final long PROXY_FROM_ENV = 180;
  private static final long TRANSPORT_SET = 184;
  private static final long T_MTU_OFF = 188;
  private static final long T_FAIRNESS_OFF = 192;
  private static final long T_GSO_OFF = 196;
  private static final long T_BIDI_OFF = 200;
  private static final long T_UNI_OFF = 208;
  private static final long T_STREAM_RECV_OFF = 216;
  private static final long T_RECV_OFF = 224;
  private static final long T_SEND_OFF = 232;
  private static final long T_IDLE_OFF = 240;
  private static final long T_KEEP_ALIVE_OFF = 248;
  private static final long T_DGRAM_RECV_OFF = 256;
  private static final long T_DGRAM_SEND_OFF = 264;
  // C ABI 6.0
  private static final long BLOB_ZSTD_LEVEL = 280;
  private static final long REUSE_LAST_PORT = 284;

  // ASTER_BLOB_ZSTD_LEVEL_* (blob_zstd_level) and aster_reuse_last_port_t
  static final int BLOB_ZSTD_LEVEL_DEFAULT = 0;
  static final int BLOB_ZSTD_LEVEL_OFF = -1;
  static final int BLOB_ZSTD_LEVEL_MAX = 19;
  static final int REUSE_LAST_PORT_DEFAULT = 0;
  static final int REUSE_LAST_PORT_ENABLED = 1;
  static final int REUSE_LAST_PORT_DISABLED = 2;

  private NativeConfig() {}

  // ── Java -> native ────────────────────────────────────────────────────

  /**
   * Build an {@code aster_config_t} in {@code arena} from {@code c} plus {@code extraAlpns}. All
   * strings and lists live in {@code arena}; the struct is valid while the arena is open.
   */
  public static MemorySegment toNative(AsterConfig c, List<byte[]> extraAlpns, Arena arena) {
    IrohLibrary lib = IrohLibrary.getInstance();
    MemorySegment seg = arena.allocate(IrohLibrary.ASTER_CONFIG);
    check(lib.configInit(seg), "aster_config_init");

    String mode = c.relayMode() == null ? "" : c.relayMode().trim().toLowerCase();
    List<String> urls = new ArrayList<>(c.relayUrls());
    int relay;
    switch (mode) {
      case "", "default" -> relay = urls.isEmpty() ? RELAY_DEFAULT : RELAY_CUSTOM;
      case "custom" -> relay = RELAY_CUSTOM;
      case "disabled" -> relay = RELAY_DISABLED;
      case "staging" -> relay = RELAY_STAGING;
      default -> {
        // A bare URL means custom (same rule as the Rust config).
        relay = RELAY_CUSTOM;
        if (urls.isEmpty()) urls.add(c.relayMode().trim());
      }
    }
    seg.set(ValueLayout.JAVA_INT, RELAY_MODE, relay);
    putList(seg, RELAY_URLS, urls.stream().map(NativeConfig::utf8).toList(), arena);
    if (c.secretKey() != null) putBytes(seg, SECRET_KEY, c.secretKey(), arena);
    putStr(seg, SECRET_KEY_FILE, c.secretKeyFile(), arena);
    putStr(seg, STORAGE_PATH, c.storagePath(), arena);
    seg.set(ValueLayout.JAVA_LONG, GC_INTERVAL_MS, Math.max(0, c.gcIntervalMs()));
    putStr(seg, BIND_ADDR, c.bindAddr(), arena);
    List<byte[]> alpns = new ArrayList<>(c.alpns());
    if (extraAlpns != null) alpns.addAll(extraAlpns);
    putList(seg, ALPNS, alpns, arena);
    putStr(seg, IDENTITY_FILE, c.identityFile(), arena);
    putStr(seg, PROXY_URL, c.proxyUrl(), arena);
    seg.set(ValueLayout.JAVA_LONG, HOOK_TIMEOUT_MS, Math.max(0, c.hookTimeoutMs()));
    seg.set(
        ValueLayout.JAVA_INT, HOOK_FAILURE_MODE, "fail_closed".equals(c.hookFailureMode()) ? 1 : 0);
    seg.set(ValueLayout.JAVA_INT, ENABLE_HOOKS, c.enableHooks() ? 1 : 0);
    seg.set(ValueLayout.JAVA_INT, ENABLE_MONITORING, c.enableMonitoring() ? 1 : 0);
    seg.set(ValueLayout.JAVA_INT, LOCAL_DISCOVERY, c.localDiscovery() ? 1 : 0);
    seg.set(ValueLayout.JAVA_INT, CLEAR_IP, c.clearIpTransports() ? 1 : 0);
    seg.set(ValueLayout.JAVA_INT, CLEAR_RELAY, c.clearRelayTransports() ? 1 : 0);
    String pm = c.portmapperConfig();
    seg.set(
        ValueLayout.JAVA_INT,
        PORTMAPPER,
        pm == null ? 0 : (pm.trim().equalsIgnoreCase("disabled") ? 2 : 1));
    seg.set(ValueLayout.JAVA_INT, PROXY_FROM_ENV, c.proxyFromEnv() ? 1 : 0);
    seg.set(ValueLayout.JAVA_INT, BLOB_ZSTD_LEVEL, blobZstdLevelToNative(c.blobZstdLevel()));
    Boolean reuse = c.reuseLastPort();
    seg.set(
        ValueLayout.JAVA_INT,
        REUSE_LAST_PORT,
        reuse == null
            ? REUSE_LAST_PORT_DEFAULT
            : (reuse ? REUSE_LAST_PORT_ENABLED : REUSE_LAST_PORT_DISABLED));

    int set = 0;
    set |= putLong(seg, T_BIDI, T_BIDI_OFF, c.transportMaxConcurrentBidiStreams());
    set |= putLong(seg, T_UNI, T_UNI_OFF, c.transportMaxConcurrentUniStreams());
    set |= putLong(seg, T_STREAM_RECV_WINDOW, T_STREAM_RECV_OFF, c.transportStreamReceiveWindow());
    set |= putLong(seg, T_RECV_WINDOW, T_RECV_OFF, c.transportReceiveWindow());
    set |= putLong(seg, T_SEND_WINDOW, T_SEND_OFF, c.transportSendWindow());
    set |= putLong(seg, T_IDLE, T_IDLE_OFF, c.transportMaxIdleTimeoutMs());
    set |= putLong(seg, T_KEEP_ALIVE, T_KEEP_ALIVE_OFF, c.transportKeepAliveIntervalMs());
    set |= putLong(seg, T_DGRAM_RECV, T_DGRAM_RECV_OFF, c.transportDatagramReceiveBufferSize());
    set |= putLong(seg, T_DGRAM_SEND, T_DGRAM_SEND_OFF, c.transportDatagramSendBufferSize());
    if (c.transportInitialMtu() != null) {
      seg.set(ValueLayout.JAVA_INT, T_MTU_OFF, c.transportInitialMtu());
      set |= T_MTU;
    }
    if (c.transportSendFairness() != null) {
      seg.set(ValueLayout.JAVA_INT, T_FAIRNESS_OFF, c.transportSendFairness() ? 1 : 0);
      set |= T_FAIRNESS;
    }
    if (c.transportEnableSegmentationOffload() != null) {
      seg.set(ValueLayout.JAVA_INT, T_GSO_OFF, c.transportEnableSegmentationOffload() ? 1 : 0);
      set |= T_GSO;
    }
    seg.set(ValueLayout.JAVA_INT, TRANSPORT_SET, set);
    return seg;
  }

  // ── native -> Java ────────────────────────────────────────────────────

  /** Overwrite every node knob of {@code b} with the values of a native config. */
  public static AsterConfig.Builder applyTo(MemorySegment seg, AsterConfig.Builder b) {
    int relay = seg.get(ValueLayout.JAVA_INT, RELAY_MODE);
    List<String> urls = getList(seg, RELAY_URLS).stream().map(NativeConfig::str).toList();
    b.relayMode(
        switch (relay) {
          case RELAY_CUSTOM -> "custom";
          case RELAY_DISABLED -> "disabled";
          case RELAY_STAGING -> "staging";
          default -> "default";
        });
    b.relayUrls(urls);
    byte[] key = getBytes(seg, SECRET_KEY);
    b.secretKey(key.length == 0 ? null : key);
    b.secretKeyFile(getStr(seg, SECRET_KEY_FILE));
    b.storagePath(getStr(seg, STORAGE_PATH));
    b.gcIntervalMs(seg.get(ValueLayout.JAVA_LONG, GC_INTERVAL_MS));
    b.bindAddr(getStr(seg, BIND_ADDR));
    b.alpns(getList(seg, ALPNS));
    b.identityFile(getStr(seg, IDENTITY_FILE));
    b.proxyUrl(getStr(seg, PROXY_URL));
    b.hookTimeoutMs((int) seg.get(ValueLayout.JAVA_LONG, HOOK_TIMEOUT_MS));
    b.hookFailureMode(
        seg.get(ValueLayout.JAVA_INT, HOOK_FAILURE_MODE) == 1 ? "fail_closed" : "fail_open");
    b.enableHooks(seg.get(ValueLayout.JAVA_INT, ENABLE_HOOKS) != 0);
    b.enableMonitoring(seg.get(ValueLayout.JAVA_INT, ENABLE_MONITORING) != 0);
    b.localDiscovery(seg.get(ValueLayout.JAVA_INT, LOCAL_DISCOVERY) != 0);
    b.clearIpTransports(seg.get(ValueLayout.JAVA_INT, CLEAR_IP) != 0);
    b.clearRelayTransports(seg.get(ValueLayout.JAVA_INT, CLEAR_RELAY) != 0);
    int pm = seg.get(ValueLayout.JAVA_INT, PORTMAPPER);
    b.portmapperConfig(pm == 0 ? null : (pm == 2 ? "disabled" : "enabled"));
    b.proxyFromEnv(seg.get(ValueLayout.JAVA_INT, PROXY_FROM_ENV) != 0);
    int level = seg.get(ValueLayout.JAVA_INT, BLOB_ZSTD_LEVEL);
    b.blobZstdLevel(
        level == BLOB_ZSTD_LEVEL_DEFAULT ? null : (level == BLOB_ZSTD_LEVEL_OFF ? 0 : level));
    int reuse = seg.get(ValueLayout.JAVA_INT, REUSE_LAST_PORT);
    b.reuseLastPort(
        reuse == REUSE_LAST_PORT_ENABLED
            ? Boolean.TRUE
            : (reuse == REUSE_LAST_PORT_DISABLED ? Boolean.FALSE : null));
    int set = seg.get(ValueLayout.JAVA_INT, TRANSPORT_SET);
    b.transportMaxConcurrentBidiStreams(getLong(seg, set, T_BIDI, T_BIDI_OFF));
    b.transportMaxConcurrentUniStreams(getLong(seg, set, T_UNI, T_UNI_OFF));
    b.transportStreamReceiveWindow(getLong(seg, set, T_STREAM_RECV_WINDOW, T_STREAM_RECV_OFF));
    b.transportReceiveWindow(getLong(seg, set, T_RECV_WINDOW, T_RECV_OFF));
    b.transportSendWindow(getLong(seg, set, T_SEND_WINDOW, T_SEND_OFF));
    b.transportMaxIdleTimeoutMs(getLong(seg, set, T_IDLE, T_IDLE_OFF));
    b.transportKeepAliveIntervalMs(getLong(seg, set, T_KEEP_ALIVE, T_KEEP_ALIVE_OFF));
    b.transportDatagramReceiveBufferSize(getLong(seg, set, T_DGRAM_RECV, T_DGRAM_RECV_OFF));
    b.transportDatagramSendBufferSize(getLong(seg, set, T_DGRAM_SEND, T_DGRAM_SEND_OFF));
    b.transportInitialMtu((set & T_MTU) != 0 ? seg.get(ValueLayout.JAVA_INT, T_MTU_OFF) : null);
    b.transportSendFairness(
        (set & T_FAIRNESS) != 0 ? seg.get(ValueLayout.JAVA_INT, T_FAIRNESS_OFF) != 0 : null);
    b.transportEnableSegmentationOffload(
        (set & T_GSO) != 0 ? seg.get(ValueLayout.JAVA_INT, T_GSO_OFF) != 0 : null);
    return b;
  }

  /** Apply the node knobs from the {@code ASTER_*} environment (Rust {@code from_env}). */
  public static AsterConfig.Builder applyEnv(AsterConfig.Builder b) {
    try (Arena arena = Arena.ofConfined()) {
      MemorySegment seg = arena.allocate(IrohLibrary.ASTER_CONFIG);
      IrohLibrary lib = IrohLibrary.getInstance();
      check(lib.configFromEnv(seg), "aster_config_from_env");
      try {
        return applyTo(seg, b);
      } finally {
        lib.configRelease(seg);
      }
    }
  }

  /**
   * Apply the node knobs from an {@code aster.toml} file, then the {@code ASTER_*} environment
   * (Rust {@code from_file}).
   */
  public static AsterConfig.Builder applyToml(Path path, AsterConfig.Builder b) {
    try (Arena arena = Arena.ofConfined()) {
      MemorySegment seg = arena.allocate(IrohLibrary.ASTER_CONFIG);
      MemorySegment p = bytesStruct(utf8(path.toString()), arena);
      IrohLibrary lib = IrohLibrary.getInstance();
      check(lib.configFromToml(p, seg), "aster_config_from_toml");
      try {
        return applyTo(seg, b);
      } finally {
        lib.configRelease(seg);
      }
    }
  }

  // ── [trust] / [enrollment] (ABI 5.1) ────────────────────────────────

  /**
   * Set the trust and enrollment fields of {@code b} from the {@code ASTER_*} environment (Rust
   * {@code AsterConfig::from_env}, {@code aster_config_enrollment_from_env}).
   */
  public static AsterConfig.Builder applyEnrollmentEnv(AsterConfig.Builder b) {
    try (Arena arena = Arena.ofConfined()) {
      MemorySegment out = arena.allocate(IrohLibrary.ASTER_BYTES);
      int status =
          (int)
              IrohLibrary.getInstance()
                  .getHandle(
                      "aster_config_enrollment_from_env",
                      java.lang.foreign.FunctionDescriptor.of(
                          ValueLayout.JAVA_INT, ValueLayout.ADDRESS))
                  .invoke(out);
      check(status, "aster_config_enrollment_from_env");
      return site.aster.enrollment.TrustJson.apply(takeOwned(out), b);
    } catch (RuntimeException e) {
      throw e;
    } catch (Throwable t) {
      throw new IrohException("aster_config_enrollment_from_env: " + t);
    }
  }

  /**
   * Set the trust and enrollment fields of {@code b} from an {@code aster.toml} file, then the
   * environment (Rust {@code AsterConfig::from_file}, {@code aster_config_enrollment_from_toml}).
   */
  public static AsterConfig.Builder applyEnrollmentToml(Path path, AsterConfig.Builder b) {
    try (Arena arena = Arena.ofConfined()) {
      MemorySegment out = arena.allocate(IrohLibrary.ASTER_BYTES);
      int status =
          (int)
              IrohLibrary.getInstance()
                  .getHandle(
                      "aster_config_enrollment_from_toml",
                      java.lang.foreign.FunctionDescriptor.of(
                          ValueLayout.JAVA_INT, IrohLibrary.ASTER_BYTES, ValueLayout.ADDRESS))
                  .invoke(bytesStruct(utf8(path.toString()), arena), out);
      check(status, "aster_config_enrollment_from_toml(" + path + ")");
      return site.aster.enrollment.TrustJson.apply(takeOwned(out), b);
    } catch (RuntimeException e) {
      throw e;
    } catch (Throwable t) {
      throw new IrohException("aster_config_enrollment_from_toml: " + t);
    }
  }

  /** Copy and release an owned {@code aster_bytes_t}. */
  private static byte[] takeOwned(MemorySegment bytes) {
    byte[] data = getBytes(bytes, 0);
    MemorySegment ptr = bytes.get(ValueLayout.ADDRESS, 0);
    if (!ptr.equals(MemorySegment.NULL)) {
      IrohLibrary.getInstance().stringRelease(ptr, bytes.get(ValueLayout.JAVA_LONG, 8));
    }
    return data;
  }

  /** A node started by {@link #startNodeEnrolled}: its handle and enrollment (0 = none). */
  public record EnrolledNode(long node, long enrollment) {}

  /**
   * Start a node as {@code AsterServer} / {@code AsterClient} do ({@code
   * aster_node_start_enrolled}, ABI 5.1): when {@code trustJson} enables enrollment Rust validates
   * the whole plan first (an {@code INVALID_ARGUMENT} naming the setting; nothing runs open),
   * starts the node with fail-closed hooks and {@code aster.enrollment/1} and installs Enrollment;
   * otherwise this is a plain {@code aster_node_start}. Blocks until the node is up.
   */
  public static EnrolledNode startNodeEnrolled(
      IrohRuntime runtime,
      AsterConfig config,
      List<byte[]> extraAlpns,
      byte[] trustJson,
      int side,
      byte[] secretKeyOverride,
      String secretKeyFileOverride) {
    long opId;
    try (Arena arena = Arena.ofConfined()) {
      MemorySegment cfg =
          toNative(config == null ? AsterConfig.builder().build() : config, extraAlpns, arena);
      if (secretKeyOverride != null) {
        putBytes(cfg, SECRET_KEY, secretKeyOverride, arena);
      }
      if (secretKeyFileOverride != null) {
        putStr(cfg, SECRET_KEY_FILE, secretKeyFileOverride, arena);
      }
      MemorySegment op = arena.allocate(ValueLayout.JAVA_LONG);
      int status =
          (int)
              IrohLibrary.getInstance()
                  .getHandle(
                      "aster_node_start_enrolled",
                      java.lang.foreign.FunctionDescriptor.of(
                          ValueLayout.JAVA_INT,
                          ValueLayout.JAVA_LONG,
                          ValueLayout.ADDRESS,
                          IrohLibrary.ASTER_BYTES,
                          ValueLayout.JAVA_INT,
                          ValueLayout.JAVA_LONG,
                          ValueLayout.ADDRESS))
                  .invoke(runtime.nativeHandle(), cfg, bytesStruct(trustJson, arena), side, 0L, op);
      check(status, "aster_node_start_enrolled");
      opId = op.get(ValueLayout.JAVA_LONG, 0);
    } catch (RuntimeException e) {
      throw e;
    } catch (Throwable t) {
      throw new IrohException("aster_node_start_enrolled: " + t);
    }
    try {
      IrohEvent event = runtime.registry().register(opId).get(120, TimeUnit.SECONDS);
      if (event.kind() != IrohEventKind.NODE_CREATED) {
        throw new IrohException("aster_node_start_enrolled failed: " + event.kind());
      }
      return new EnrolledNode(event.handle(), event.related());
    } catch (InterruptedException e) {
      Thread.currentThread().interrupt();
      throw new IrohException("aster_node_start_enrolled interrupted");
    } catch (ExecutionException e) {
      Throwable cause = e.getCause();
      if (cause instanceof IrohException ie) {
        throw IrohException.forStatus(
            ie.status(), "aster_node_start_enrolled failed: " + ie.getMessage());
      }
      throw new IrohException("aster_node_start_enrolled failed: " + cause.getMessage());
    } catch (TimeoutException e) {
      throw new IrohException("aster_node_start_enrolled timed out");
    }
  }

  // ── node start ────────────────────────────────────────────────────────

  /**
   * Start a node on {@code runtime} from {@code config} (null = defaults) plus {@code extraAlpns},
   * optionally gated by {@code gate} (0 = none). Blocks until the node is up; returns its handle.
   */
  public static long startNode(
      IrohRuntime runtime, AsterConfig config, List<byte[]> extraAlpns, long gate) {
    return startNode(runtime, config, extraAlpns, gate, null);
  }

  /** As {@link #startNode(IrohRuntime, AsterConfig, List, long)}, overriding the secret key. */
  public static long startNode(
      IrohRuntime runtime,
      AsterConfig config,
      List<byte[]> extraAlpns,
      long gate,
      byte[] secretKeyOverride) {
    IrohLibrary lib = IrohLibrary.getInstance();
    long opId;
    try (Arena arena = Arena.ofConfined()) {
      MemorySegment cfg =
          toNative(config == null ? AsterConfig.builder().build() : config, extraAlpns, arena);
      if (secretKeyOverride != null) {
        putBytes(cfg, SECRET_KEY, secretKeyOverride, arena);
      }
      MemorySegment op = arena.allocate(ValueLayout.JAVA_LONG);
      check(lib.nodeStartAsync(runtime.nativeHandle(), cfg, gate, op), "aster_node_start");
      opId = op.get(ValueLayout.JAVA_LONG, 0);
    }
    try {
      IrohEvent event = runtime.registry().register(opId).get(60, TimeUnit.SECONDS);
      if (event.kind() != IrohEventKind.NODE_CREATED) {
        throw new IrohException("aster_node_start failed: " + event.kind());
      }
      return event.handle();
    } catch (InterruptedException e) {
      Thread.currentThread().interrupt();
      throw new IrohException("aster_node_start interrupted");
    } catch (ExecutionException e) {
      throw new IrohException("aster_node_start failed: " + e.getCause().getMessage());
    } catch (TimeoutException e) {
      throw new IrohException("aster_node_start timed out");
    }
  }

  // ── helpers ───────────────────────────────────────────────────────────

  /** {@code null} = default (0), {@code 0} = off (-1), a level clamped to 1..=19. */
  static int blobZstdLevelToNative(Integer level) {
    if (level == null) return BLOB_ZSTD_LEVEL_DEFAULT;
    if (level < 0) {
      throw new IllegalArgumentException(
          "blobZstdLevel must be 0 (off) or a zstd level, got " + level);
    }
    return level == 0 ? BLOB_ZSTD_LEVEL_OFF : Math.min(level, BLOB_ZSTD_LEVEL_MAX);
  }

  static void check(int status, String what) {
    if (status != 0) {
      throw IrohException.forStatus(IrohStatus.fromCode(status), what + " failed: " + lastError());
    }
  }

  /** The thread's last FFI error message (empty if none). */
  public static String lastError() {
    try (Arena arena = Arena.ofConfined()) {
      MemorySegment buf = arena.allocate(1024);
      long n =
          (long)
              IrohLibrary.getInstance()
                  .getHandle(
                      "aster_last_error_message",
                      java.lang.foreign.FunctionDescriptor.of(
                          ValueLayout.JAVA_LONG, ValueLayout.ADDRESS, ValueLayout.JAVA_LONG))
                  .invoke(buf, 1023L);
      return new String(
          buf.asSlice(0, Math.min(n, 1023)).toArray(ValueLayout.JAVA_BYTE), StandardCharsets.UTF_8);
    } catch (Throwable t) {
      return "";
    }
  }

  static byte[] utf8(String s) {
    return s.getBytes(StandardCharsets.UTF_8);
  }

  static String str(byte[] b) {
    return new String(b, StandardCharsets.UTF_8);
  }

  /** An {@code aster_bytes_t} value (16 bytes) pointing at a copy of {@code data}. */
  public static MemorySegment bytesStruct(byte[] data, Arena arena) {
    MemorySegment s = arena.allocate(IrohLibrary.ASTER_BYTES);
    if (data != null && data.length > 0) {
      MemorySegment d = arena.allocate(data.length);
      d.copyFrom(MemorySegment.ofArray(data));
      s.set(ValueLayout.ADDRESS, 0, d);
      s.set(ValueLayout.JAVA_LONG, 8, data.length);
    }
    return s;
  }

  /** An {@code aster_bytes_list_t} value (16 bytes) over copies of {@code items}. */
  public static MemorySegment listStruct(List<byte[]> items, Arena arena) {
    MemorySegment s = arena.allocate(IrohLibrary.ASTER_BYTES_LIST);
    if (items != null && !items.isEmpty()) {
      MemorySegment arr = arena.allocate(16L * items.size(), 8);
      for (int i = 0; i < items.size(); i++) {
        arr.asSlice(16L * i, 16).copyFrom(bytesStruct(items.get(i), arena));
      }
      s.set(ValueLayout.ADDRESS, 0, arr);
      s.set(ValueLayout.JAVA_LONG, 8, items.size());
    }
    return s;
  }

  private static void putBytes(MemorySegment seg, long off, byte[] data, Arena arena) {
    seg.asSlice(off, 16).copyFrom(bytesStruct(data, arena));
  }

  private static void putStr(MemorySegment seg, long off, String s, Arena arena) {
    if (s != null && !s.isEmpty()) putBytes(seg, off, utf8(s), arena);
  }

  private static void putList(MemorySegment seg, long off, List<byte[]> items, Arena arena) {
    seg.asSlice(off, 16).copyFrom(listStruct(items, arena));
  }

  private static int putLong(MemorySegment seg, int bit, long off, Long v) {
    if (v == null) return 0;
    seg.set(ValueLayout.JAVA_LONG, off, v);
    return bit;
  }

  private static Long getLong(MemorySegment seg, int set, int bit, long off) {
    return (set & bit) != 0 ? seg.get(ValueLayout.JAVA_LONG, off) : null;
  }

  private static byte[] getBytes(MemorySegment seg, long off) {
    MemorySegment ptr = seg.get(ValueLayout.ADDRESS, off);
    long len = seg.get(ValueLayout.JAVA_LONG, off + 8);
    if (ptr.equals(MemorySegment.NULL) || len == 0) return new byte[0];
    return ptr.reinterpret(len).toArray(ValueLayout.JAVA_BYTE);
  }

  private static String getStr(MemorySegment seg, long off) {
    byte[] b = getBytes(seg, off);
    return b.length == 0 ? null : str(b);
  }

  private static List<byte[]> getList(MemorySegment seg, long off) {
    MemorySegment items = seg.get(ValueLayout.ADDRESS, off);
    long len = seg.get(ValueLayout.JAVA_LONG, off + 8);
    List<byte[]> out = new ArrayList<>();
    if (items.equals(MemorySegment.NULL) || len == 0) return out;
    MemorySegment arr = items.reinterpret(16 * len);
    for (int i = 0; i < len; i++) {
      out.add(getBytes(arr, 16L * i));
    }
    return out;
  }
}
