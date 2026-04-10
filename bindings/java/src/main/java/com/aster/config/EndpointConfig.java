package com.aster.config;

import com.aster.ffi.IrohLibrary;
import com.aster.ffi.IrohRelayMode;
import java.lang.foreign.MemoryLayout;
import java.lang.foreign.MemorySegment;
import java.lang.foreign.SegmentAllocator;
import java.lang.foreign.ValueLayout;
import java.lang.invoke.VarHandle;
import java.nio.charset.StandardCharsets;
import java.util.List;

/**
 * Builder for {@code iroh_endpoint_config_t}.
 *
 * <p>Call {@link #toNative(SegmentAllocator)} to produce a native struct suitable for {@code
 * iroh_endpoint_create}.
 */
public class EndpointConfig {

  private static final MemoryLayout LAYOUT = IrohLibrary.IROH_ENDPOINT_CONFIG;
  private static final VarHandle VH_STRUCT_SIZE =
      LAYOUT.varHandle(MemoryLayout.PathElement.groupElement("struct_size"));
  private static final VarHandle VH_RELAY_MODE =
      LAYOUT.varHandle(MemoryLayout.PathElement.groupElement("relay_mode"));
  private static final VarHandle VH_ENABLE_DISCOVERY =
      LAYOUT.varHandle(MemoryLayout.PathElement.groupElement("enable_discovery"));
  private static final VarHandle VH_ENABLE_HOOKS =
      LAYOUT.varHandle(MemoryLayout.PathElement.groupElement("enable_hooks"));
  private static final VarHandle VH_HOOK_TIMEOUT_MS =
      LAYOUT.varHandle(MemoryLayout.PathElement.groupElement("hook_timeout_ms"));
  private static final VarHandle VH_CLEAR_IP_TRANSPORTS =
      LAYOUT.varHandle(MemoryLayout.PathElement.groupElement("clear_ip_transports"));
  private static final VarHandle VH_CLEAR_RELAY_TRANSPORTS =
      LAYOUT.varHandle(MemoryLayout.PathElement.groupElement("clear_relay_transports"));
  private static final VarHandle VH_PORTMAPPER_CONFIG =
      LAYOUT.varHandle(MemoryLayout.PathElement.groupElement("portmapper_config"));
  private static final VarHandle VH_PROXY_FROM_ENV =
      LAYOUT.varHandle(MemoryLayout.PathElement.groupElement("proxy_from_env"));

  private static final VarHandle VH_ALPNS_ITEMS =
      LAYOUT.varHandle(
          MemoryLayout.PathElement.groupElement("alpns"),
          MemoryLayout.PathElement.groupElement("items"));
  private static final VarHandle VH_ALPNS_LEN =
      LAYOUT.varHandle(
          MemoryLayout.PathElement.groupElement("alpns"),
          MemoryLayout.PathElement.groupElement("len"));
  private static final VarHandle VH_RELAY_URLS_ITEMS =
      LAYOUT.varHandle(
          MemoryLayout.PathElement.groupElement("relay_urls"),
          MemoryLayout.PathElement.groupElement("items"));
  private static final VarHandle VH_RELAY_URLS_LEN =
      LAYOUT.varHandle(
          MemoryLayout.PathElement.groupElement("relay_urls"),
          MemoryLayout.PathElement.groupElement("len"));

  private static final VarHandle VH_BYTES_PTR =
      IrohLibrary.IROH_BYTES.varHandle(MemoryLayout.PathElement.groupElement("ptr"));
  private static final VarHandle VH_BYTES_LEN =
      IrohLibrary.IROH_BYTES.varHandle(MemoryLayout.PathElement.groupElement("len"));
  private static final VarHandle VH_LIST_ITEMS =
      IrohLibrary.IROH_BYTES_LIST.varHandle(MemoryLayout.PathElement.groupElement("items"));
  private static final VarHandle VH_LIST_LEN =
      IrohLibrary.IROH_BYTES_LIST.varHandle(MemoryLayout.PathElement.groupElement("len"));

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

  /**
   * Encode this config into a native {@code iroh_endpoint_config_t} struct allocated from {@code
   * allocator}.
   */
  public MemorySegment toNative(SegmentAllocator alloc) {
    MemorySegment seg = alloc.allocate(LAYOUT);

    // Use MemorySegment.set(...) directly to avoid VarHandle varargs arity issues
    seg.set(ValueLayout.JAVA_INT, 0, (int) LAYOUT.byteSize());
    seg.set(ValueLayout.JAVA_INT, 4, (int) relayMode);
    seg.set(ValueLayout.JAVA_INT, 56, enableDiscovery ? 1 : 0);
    seg.set(ValueLayout.JAVA_INT, 60, enableHooks ? 1 : 0);
    seg.set(ValueLayout.JAVA_LONG, 64, hookTimeoutMs);
    seg.set(ValueLayout.JAVA_INT, 88, clearIpTransports ? 1 : 0);
    seg.set(ValueLayout.JAVA_INT, 92, clearRelayTransports ? 1 : 0);
    seg.set(ValueLayout.JAVA_INT, 96, portmapperConfig);
    seg.set(ValueLayout.JAVA_INT, 120, proxyFromEnv ? 1 : 0);

    // secret_key: allocate bytes, store ptr+len into the nested IROH_BYTES sub-region
    if (secretKey.length > 0) {
      MemorySegment dataSeg = alloc.allocate(secretKey.length);
      dataSeg.copyFrom(MemorySegment.ofArray(secretKey));
      MemorySegment secretKeySeg = seg.asSlice(8, 16); // IROH_BYTES at offset 8
      VH_BYTES_PTR.set(secretKeySeg, dataSeg);
      VH_BYTES_LEN.set(secretKeySeg, (long) secretKey.length);
    }

    // alpns: allocate array of iroh_bytes_t, fill, store ptr+len
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
      VH_ALPNS_ITEMS.set(seg, listSeg);
      VH_ALPNS_LEN.set(seg, (long) alpns.size());
    }

    // relay_urls: same pattern
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
      VH_RELAY_URLS_ITEMS.set(seg, listSeg);
      VH_RELAY_URLS_LEN.set(seg, (long) relayUrls.size());
    }

    // bind_addr: allocate string bytes, store ptr+len into the nested IROH_BYTES sub-region
    if (bindAddr.length > 0) {
      MemorySegment dataSeg = alloc.allocate(bindAddr.length);
      dataSeg.copyFrom(MemorySegment.ofArray(bindAddr));
      MemorySegment bindAddrSeg = seg.asSlice(72, 16); // IROH_BYTES at offset 72
      VH_BYTES_PTR.set(bindAddrSeg, dataSeg);
      VH_BYTES_LEN.set(bindAddrSeg, (long) bindAddr.length);
    }

    // proxy_url: allocate string bytes, store ptr+len into the nested IROH_BYTES sub-region
    if (proxyUrl.length > 0) {
      MemorySegment dataSeg = alloc.allocate(proxyUrl.length);
      dataSeg.copyFrom(MemorySegment.ofArray(proxyUrl));
      MemorySegment proxyUrlSeg = seg.asSlice(104, 16); // IROH_BYTES at offset 104
      VH_BYTES_PTR.set(proxyUrlSeg, dataSeg);
      VH_BYTES_LEN.set(proxyUrlSeg, (long) proxyUrl.length);
    }

    return seg;
  }
}
