package site.aster.config;

import java.lang.foreign.MemorySegment;
import java.lang.foreign.SegmentAllocator;
import java.lang.foreign.ValueLayout;
import java.nio.charset.StandardCharsets;
import java.util.List;
import site.aster.ffi.IrohRelayMode;

/**
 * Builder for {@code iroh_endpoint_config_t}.
 *
 * <p>Call {@link #toNative(SegmentAllocator)} to produce a native struct suitable for {@code
 * iroh_endpoint_create}.
 *
 * <p>All fields are set via direct memory access (segment.set) to avoid VarHandle issues with
 * nested structs in Java 25 FFM.
 */
public class EndpointConfig {

  // iroh_endpoint_config_t layout (verified against Rust iroh_endpoint_config_t):
  // struct_size: 0 (INT)
  // relay_mode: 4 (INT)
  // secret_key: 8 (IROH_BYTES: ptr at +0, len at +8) → 16 bytes
  // alpns: 24 (IROH_BYTES_LIST: items at +0, len at +8) → 16 bytes
  // relay_urls: 40 (IROH_BYTES_LIST: items at +0, len at +8) → 16 bytes
  // enable_discovery: 56 (INT)
  // enable_hooks: 60 (INT)
  // hook_timeout_ms: 64 (LONG)
  // bind_addr: 72 (IROH_BYTES: ptr at +0, len at +8) → 16 bytes
  // clear_ip_transports: 88 (INT)
  // clear_relay_transports: 92 (INT)
  // portmapper_config: 96 (INT)
  // proxy_url: 104 (IROH_BYTES: ptr at +0, len at +8) → 16 bytes
  // proxy_from_env: 120 (INT)
  // data_dir_utf8: 128 (IROH_BYTES: ptr at +0, len at +8) → 16 bytes

  private int relayMode = 0; // 0=default
  private byte[] secretKey = new byte[0];
  private List<String> alpns = List.of();
  private List<String> relayUrls = List.of();
  private boolean enableDiscovery = true;
  private boolean enableHooks = false;
  private long hookTimeoutMs = 5000;
  private byte[] bindAddr = new byte[0];
  private boolean clearIpTransports = false;
  private boolean clearRelayTransports = false;
  private int portmapperConfig = 0; // 0=enabled
  private byte[] proxyUrl = new byte[0];
  private boolean proxyFromEnv = false;
  private byte[] dataDir = new byte[0];

  public EndpointConfig relayMode(int mode) {
    this.relayMode = mode;
    return this;
  }

  /** Set relay mode using the typed enum. */
  public EndpointConfig relayMode(IrohRelayMode mode) {
    this.relayMode = mode.code;
    return this;
  }

  public EndpointConfig secretKey(byte[] seed) {
    this.secretKey = seed;
    return this;
  }

  public EndpointConfig alpns(List<String> alpns) {
    this.alpns = alpns;
    return this;
  }

  public EndpointConfig relayUrls(List<String> urls) {
    this.relayUrls = urls;
    return this;
  }

  public EndpointConfig enableDiscovery(boolean enable) {
    this.enableDiscovery = enable;
    return this;
  }

  public EndpointConfig enableHooks(boolean enable) {
    this.enableHooks = enable;
    return this;
  }

  public EndpointConfig hookTimeoutMs(long ms) {
    this.hookTimeoutMs = ms;
    return this;
  }

  public EndpointConfig bindAddr(String addr) {
    this.bindAddr = addr.getBytes(StandardCharsets.UTF_8);
    return this;
  }

  public EndpointConfig clearIpTransports(boolean clear) {
    this.clearIpTransports = clear;
    return this;
  }

  public EndpointConfig clearRelayTransports(boolean clear) {
    this.clearRelayTransports = clear;
    return this;
  }

  public EndpointConfig portmapperDisabled(boolean disabled) {
    this.portmapperConfig = disabled ? 1 : 0;
    return this;
  }

  public EndpointConfig proxyUrl(String url) {
    this.proxyUrl = url.getBytes(StandardCharsets.UTF_8);
    return this;
  }

  public EndpointConfig proxyFromEnv(boolean fromEnv) {
    this.proxyFromEnv = fromEnv;
    return this;
  }

  public EndpointConfig dataDir(String path) {
    this.dataDir = path.getBytes(StandardCharsets.UTF_8);
    return this;
  }

  /**
   * Encode this config into a native {@code iroh_endpoint_config_t} struct allocated from {@code
   * allocator}.
   *
   * <p>Uses direct memory access (segment.set) instead of VarHandle to avoid Java 25 FFM issues
   * with nested struct field access.
   */
  public MemorySegment toNative(SegmentAllocator alloc) {
    MemorySegment seg = alloc.allocate(144);

    // struct_size at 0
    seg.set(ValueLayout.JAVA_INT, 0, 144);
    // relay_mode at 4
    seg.set(ValueLayout.JAVA_INT, 4, relayMode);
    // enable_discovery at 56
    seg.set(ValueLayout.JAVA_INT, 56, enableDiscovery ? 1 : 0);
    // enable_hooks at 60
    seg.set(ValueLayout.JAVA_INT, 60, enableHooks ? 1 : 0);
    // hook_timeout_ms at 64
    seg.set(ValueLayout.JAVA_LONG, 64, hookTimeoutMs);
    // clear_ip_transports at 88
    seg.set(ValueLayout.JAVA_INT, 88, clearIpTransports ? 1 : 0);
    // clear_relay_transports at 92
    seg.set(ValueLayout.JAVA_INT, 92, clearRelayTransports ? 1 : 0);
    // portmapper_config at 96
    seg.set(ValueLayout.JAVA_INT, 96, portmapperConfig);
    // proxy_from_env at 120
    seg.set(ValueLayout.JAVA_INT, 120, proxyFromEnv ? 1 : 0);

    // secret_key at 8: ptr at +0, len at +8
    if (secretKey.length > 0) {
      MemorySegment dataSeg = alloc.allocate(secretKey.length);
      dataSeg.copyFrom(MemorySegment.ofArray(secretKey));
      seg.set(ValueLayout.ADDRESS, 8, dataSeg);
      seg.set(ValueLayout.JAVA_LONG, 16, (long) secretKey.length);
    }

    // alpns at 24: items ptr at +0, len at +8
    if (!alpns.isEmpty()) {
      MemorySegment listSeg = alloc.allocate(16L * alpns.size());
      for (int i = 0; i < alpns.size(); i++) {
        byte[] b = alpns.get(i).getBytes(StandardCharsets.UTF_8);
        MemorySegment itemSeg = alloc.allocate(b.length);
        itemSeg.copyFrom(MemorySegment.ofArray(b));
        long itemOffset = 16L * i;
        listSeg.set(ValueLayout.ADDRESS, itemOffset, itemSeg);
        listSeg.set(ValueLayout.JAVA_LONG, itemOffset + 8, (long) b.length);
      }
      seg.set(ValueLayout.ADDRESS, 24, listSeg);
      seg.set(ValueLayout.JAVA_LONG, 32, (long) alpns.size());
    }

    // relay_urls at 40: items ptr at +0, len at +8
    if (!relayUrls.isEmpty()) {
      MemorySegment listSeg = alloc.allocate(16L * relayUrls.size());
      for (int i = 0; i < relayUrls.size(); i++) {
        byte[] b = relayUrls.get(i).getBytes(StandardCharsets.UTF_8);
        MemorySegment itemSeg = alloc.allocate(b.length);
        itemSeg.copyFrom(MemorySegment.ofArray(b));
        long itemOffset = 16L * i;
        listSeg.set(ValueLayout.ADDRESS, itemOffset, itemSeg);
        listSeg.set(ValueLayout.JAVA_LONG, itemOffset + 8, (long) b.length);
      }
      seg.set(ValueLayout.ADDRESS, 40, listSeg);
      seg.set(ValueLayout.JAVA_LONG, 48, (long) relayUrls.size());
    }

    // bind_addr at 72: ptr at +0, len at +8
    if (bindAddr.length > 0) {
      MemorySegment dataSeg = alloc.allocate(bindAddr.length);
      dataSeg.copyFrom(MemorySegment.ofArray(bindAddr));
      seg.set(ValueLayout.ADDRESS, 72, dataSeg);
      seg.set(ValueLayout.JAVA_LONG, 80, (long) bindAddr.length);
    }

    // proxy_url at 104: ptr at +0, len at +8
    if (proxyUrl.length > 0) {
      MemorySegment dataSeg = alloc.allocate(proxyUrl.length);
      dataSeg.copyFrom(MemorySegment.ofArray(proxyUrl));
      seg.set(ValueLayout.ADDRESS, 104, dataSeg);
      seg.set(ValueLayout.JAVA_LONG, 112, (long) proxyUrl.length);
    }

    // data_dir_utf8 at 128: ptr at +0, len at +8
    if (dataDir.length > 0) {
      MemorySegment dataSeg = alloc.allocate(dataDir.length);
      dataSeg.copyFrom(MemorySegment.ofArray(dataDir));
      seg.set(ValueLayout.ADDRESS, 128, dataSeg);
      seg.set(ValueLayout.JAVA_LONG, 136, (long) dataDir.length);
    }

    return seg;
  }
}
