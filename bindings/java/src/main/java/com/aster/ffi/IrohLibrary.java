package com.aster.ffi;

import java.lang.foreign.Arena;
import java.lang.foreign.FunctionDescriptor;
import java.lang.foreign.Linker;
import java.lang.foreign.MemoryLayout;
import java.lang.foreign.MemorySegment;
import java.lang.foreign.SegmentAllocator;
import java.lang.foreign.SymbolLookup;
import java.lang.foreign.ValueLayout;
import java.lang.invoke.MethodHandle;
import java.util.Optional;

/**
 * Loads the native iroh library and provides typed {@link MethodHandle} access to all FFI functions
 * defined in the C ABI.
 *
 * <p>Uses {@code SymbolLookup.libraryLookup} to load the native library and resolve symbols.
 */
public final class IrohLibrary implements SymbolLookup {

  private static volatile IrohLibrary INSTANCE;

  private static final String LIB_PATH =
      System.getenv("IROH_LIB_PATH") != null
          ? System.getenv("IROH_LIB_PATH")
          : "/Users/emrul/dev/emrul/iroh-python/target/release/libaster_transport_ffi.dylib";

  static {
    System.load(LIB_PATH);
  }

  private final SymbolLookup impl;
  private final Arena arena;
  private final SegmentAllocator allocator;
  private final MemorySegment runtimeHandleSegment;

  private IrohLibrary() {
    this.arena = Arena.ofAuto();
    this.impl = SymbolLookup.libraryLookup(LIB_PATH, arena);
    this.allocator = arena; // Arena implements SegmentAllocator
    this.runtimeHandleSegment = allocator.allocate(ValueLayout.JAVA_LONG);
  }

  public static IrohLibrary getInstance() {
    if (INSTANCE == null) {
      synchronized (IrohLibrary.class) {
        if (INSTANCE == null) {
          INSTANCE = new IrohLibrary();
        }
      }
    }
    return INSTANCE;
  }

  @Override
  public Optional<MemorySegment> find(String name) {
    return impl.find(name);
  }

  /** Returns a typed {@link MethodHandle} for the given FFI function. */
  public MethodHandle getHandle(String name, FunctionDescriptor desc) {
    MemorySegment symbol =
        impl.find(name)
            .orElseThrow(() -> new UnsatisfiedLinkError("iroh symbol not found: " + name));
    return Linker.nativeLinker().downcallHandle(symbol, desc);
  }

  // --- Version ---

  public int abiVersionMajor() {
    try {
      return (int)
          getHandle("iroh_abi_version_major", FunctionDescriptor.of(ValueLayout.JAVA_INT)).invoke();
    } catch (Throwable t) {
      throw new AssertionError(t);
    }
  }

  public int abiVersionMinor() {
    try {
      return (int)
          getHandle("iroh_abi_version_minor", FunctionDescriptor.of(ValueLayout.JAVA_INT)).invoke();
    } catch (Throwable t) {
      throw new AssertionError(t);
    }
  }

  public int abiVersionPatch() {
    try {
      return (int)
          getHandle("iroh_abi_version_patch", FunctionDescriptor.of(ValueLayout.JAVA_INT)).invoke();
    } catch (Throwable t) {
      throw new AssertionError(t);
    }
  }

  // --- Node identity ---

  /**
   * Get the node ID (bytes) for an endpoint.
   *
   * @param runtimeHandle the runtime handle
   * @param endpointHandle the endpoint handle
   * @param outBuf the output buffer to write the node ID into
   * @param capacity the capacity of the output buffer
   * @param outLen where to write the actual node ID length
   * @return status code (0 = OK)
   */
  public int endpointId(
      long runtimeHandle,
      long endpointHandle,
      MemorySegment outBuf,
      long capacity,
      MemorySegment outLen) {
    try {
      return (int)
          getHandle(
                  "iroh_endpoint_id",
                  FunctionDescriptor.of(
                      ValueLayout.JAVA_INT,
                      ValueLayout.JAVA_LONG,
                      ValueLayout.JAVA_LONG,
                      ValueLayout.ADDRESS,
                      ValueLayout.JAVA_LONG,
                      ValueLayout.ADDRESS))
              .invoke(runtimeHandle, endpointHandle, outBuf, capacity, outLen);
    } catch (Throwable t) {
      throw new AssertionError(t);
    }
  }

  public SegmentAllocator allocator() {
    return allocator;
  }

  /** Arena shared across all FFI operations for reinterpreted native pointers. */
  public Arena sharedArena() {
    return arena;
  }

  /** Pre-allocated segment for runtime handle output (written by iroh_runtime_new). */
  public MemorySegment runtimeHandleSegment() {
    return runtimeHandleSegment;
  }

  // --- Struct layouts (verified against Rust #[repr(C)] structs in ffi/src/lib.rs) ---

  public static final MemoryLayout IROH_BYTES =
      MemoryLayout.structLayout(
          ValueLayout.ADDRESS.withName("ptr"), // 0
          ValueLayout.JAVA_LONG.withName("len") // 8
          );

  public static final MemoryLayout IROH_BYTES_LIST =
      MemoryLayout.structLayout(
          ValueLayout.ADDRESS.withName("items"), // 0
          ValueLayout.JAVA_LONG.withName("len") // 8
          );

  /**
   * iroh_runtime_config_t: struct_size, worker_threads, event_queue_capacity, reserved. 16 bytes.
   */
  public static final MemoryLayout IROH_RUNTIME_CONFIG =
      MemoryLayout.structLayout(
          ValueLayout.JAVA_INT.withName("struct_size"), // 0
          ValueLayout.JAVA_INT.withName("worker_threads"), // 4
          ValueLayout.JAVA_INT.withName("event_queue_capacity"), // 8
          ValueLayout.JAVA_INT.withName("reserved") // 12
          );

  /**
   * iroh_endpoint_config_t: struct_size, relay_mode, secret_key, alpns, relay_urls,
   * enable_discovery, enable_hooks, hook_timeout_ms, bind_addr, clear_ip_transports,
   * clear_relay_transports, portmapper_config, proxy_url, proxy_from_env.
   *
   * <p>Total: 120 bytes. Matches Rust iroh_endpoint_config_t exactly.
   */
  public static final MemoryLayout IROH_ENDPOINT_CONFIG =
      MemoryLayout.structLayout(
          ValueLayout.JAVA_INT.withName("struct_size"), // 0
          ValueLayout.JAVA_INT.withName("relay_mode"), // 4
          IROH_BYTES.withName("secret_key"), // 8  (ptr+len = 16 bytes)
          IROH_BYTES_LIST.withName("alpns"), // 24 (items+len = 16 bytes)
          IROH_BYTES_LIST.withName("relay_urls"), // 40 (items+len = 16 bytes)
          ValueLayout.JAVA_INT.withName("enable_discovery"), // 56
          ValueLayout.JAVA_INT.withName("enable_hooks"), // 60
          ValueLayout.JAVA_LONG.withName("hook_timeout_ms"), // 64
          IROH_BYTES.withName("bind_addr"), // 72 (ptr+len = 16 bytes)
          ValueLayout.JAVA_INT.withName("clear_ip_transports"), // 88
          ValueLayout.JAVA_INT.withName("clear_relay_transports"), // 92
          ValueLayout.JAVA_INT.withName("portmapper_config"), // 96
          MemoryLayout.paddingLayout(4), // 4 bytes padding → proxy_url at 104 (8-byte aligned)
          IROH_BYTES.withName("proxy_url"), // 104 (ptr+len = 16 bytes)
          ValueLayout.JAVA_INT.withName("proxy_from_env") // 120
          );

  /**
   * iroh_connect_config_t: struct_size, flags, node_id (hex string), alpn, addr. Total: 48 bytes.
   * addr is *const iroh_node_addr_t, passed as NULL for simple connect.
   */
  public static final MemoryLayout IROH_CONNECT_CONFIG =
      MemoryLayout.structLayout(
          ValueLayout.JAVA_INT.withName("struct_size"), // 0
          ValueLayout.JAVA_INT.withName("flags"), // 4
          IROH_BYTES.withName("node_id"), // 8  (ptr+len = 16 bytes)
          IROH_BYTES.withName("alpn"), // 24 (ptr+len = 16 bytes)
          ValueLayout.ADDRESS.withName("addr") // 40
          );

  /**
   * iroh_event_t: matches Rust iroh_event_t exactly. Total: 80 bytes.
   *
   * <p>Note: 4 bytes of padding at offset 12 between status and operation (u64 alignment).
   */
  public static final MemoryLayout IROH_EVENT =
      MemoryLayout.structLayout(
          ValueLayout.JAVA_INT.withName("struct_size"), // 0
          ValueLayout.JAVA_INT.withName("kind"), // 4
          ValueLayout.JAVA_INT.withName("status"), // 8
          MemoryLayout.paddingLayout(4), // 12 — alignment padding
          ValueLayout.JAVA_LONG.withName("operation"), // 16
          ValueLayout.JAVA_LONG.withName("handle"), // 24
          ValueLayout.JAVA_LONG.withName("related"), // 32
          ValueLayout.JAVA_LONG.withName("user_data"), // 40
          ValueLayout.ADDRESS.withName("data_ptr"), // 48
          ValueLayout.JAVA_LONG.withName("data_len"), // 56
          ValueLayout.JAVA_LONG.withName("buffer"), // 64
          ValueLayout.JAVA_INT.withName("error_code"), // 72
          ValueLayout.JAVA_INT.withName("flags") // 76
          );
}
