package site.aster.node;

import java.lang.foreign.Arena;
import java.lang.foreign.MemorySegment;
import java.lang.foreign.ValueLayout;
import java.nio.charset.StandardCharsets;
import java.util.List;
import site.aster.ffi.IrohException;
import site.aster.ffi.IrohLibrary;
import site.aster.ffi.IrohStatus;
import site.aster.ffi.NativeConfig;
import site.aster.handle.IrohRuntime;

/**
 * Gate-0 admission policy (the Rust {@code GatePolicy}, C ABI {@code aster_gate_*}).
 *
 * <p>Pass it to {@link IrohNode#start(site.aster.config.AsterConfig, List, Gate)} (or {@link
 * IrohNode#setGate}) and the node admits inbound peers in Rust: the admission ALPNs are open to
 * everyone; any other ALPN needs an {@link #admit admitted} peer (or {@link TrustMode#OPEN_DEV});
 * rejected peers are closed with the {@link #reject reject} code and reason.
 *
 * <p>A gate belongs to one native runtime; nodes gated by it run on that runtime.
 */
public final class Gate implements AutoCloseable {

  /** Gate-0 trust posture ({@code aster_trust_mode_t}). */
  public enum TrustMode {
    /** Unknown peers may use only the admission ALPNs (default). */
    PROTECTED,
    /** Local/dev: unknown peers may use any ALPN. */
    OPEN_DEV
  }

  private final IrohRuntime runtime;
  private final long handle;
  private volatile boolean closed;

  private Gate(IrohRuntime runtime, long handle) {
    this.runtime = runtime;
    this.handle = handle;
  }

  /** A PROTECTED gate (admission ALPNs open, reject 403 "peer not admitted") on a new runtime. */
  public static Gate create() {
    IrohRuntime runtime = IrohRuntime.create();
    try (Arena arena = Arena.ofConfined()) {
      MemorySegment out = arena.allocate(ValueLayout.JAVA_LONG);
      check(IrohLibrary.getInstance().gateNew(runtime.nativeHandle(), out), "aster_gate_new");
      return new Gate(runtime, out.get(ValueLayout.JAVA_LONG, 0));
    }
  }

  public Gate trustMode(TrustMode mode) {
    check(
        IrohLibrary.getInstance().gateSetTrustMode(runtime.nativeHandle(), handle, mode.ordinal()),
        "aster_gate_set_trust_mode");
    return this;
  }

  /** Replace the ALPNs that stay open to peers that are not admitted. */
  public Gate admissionAlpns(List<byte[]> alpns) {
    try (Arena arena = Arena.ofConfined()) {
      check(
          IrohLibrary.getInstance()
              .gateSetAdmissionAlpns(
                  runtime.nativeHandle(), handle, NativeConfig.listStruct(alpns, arena)),
          "aster_gate_set_admission_alpns");
    }
    return this;
  }

  /** QUIC close code and reason sent to rejected peers. */
  public Gate reject(int code, String reason) {
    try (Arena arena = Arena.ofConfined()) {
      check(
          IrohLibrary.getInstance()
              .gateSetReject(
                  runtime.nativeHandle(),
                  handle,
                  code,
                  NativeConfig.bytesStruct(reason.getBytes(StandardCharsets.UTF_8), arena)),
          "aster_gate_set_reject");
    }
    return this;
  }

  /** Admit {@code peer} (64-hex node id). */
  public void admit(String peer) {
    peerOp("aster_gate_admit", peer);
  }

  /** Revoke {@code peer}'s admission (new connections are gated again). */
  public void revoke(String peer) {
    peerOp("aster_gate_revoke", peer);
  }

  public boolean isAdmitted(String peer) {
    try (Arena arena = Arena.ofConfined()) {
      MemorySegment out = arena.allocate(ValueLayout.JAVA_INT);
      check(
          IrohLibrary.getInstance()
              .gateIsAdmitted(runtime.nativeHandle(), handle, peerBytes(peer, arena), out),
          "aster_gate_is_admitted");
      return out.get(ValueLayout.JAVA_INT, 0) != 0;
    }
  }

  IrohRuntime runtime() {
    return runtime;
  }

  long handle() {
    if (closed) throw new IllegalStateException("gate closed");
    return handle;
  }

  /** Free the gate handle; nodes using it keep it until they close. */
  @Override
  public void close() {
    if (!closed) {
      closed = true;
      IrohLibrary.getInstance().gateFree(runtime.nativeHandle(), handle);
    }
  }

  private void peerOp(String fn, String peer) {
    try (Arena arena = Arena.ofConfined()) {
      check(
          IrohLibrary.getInstance()
              .gatePeerOp(fn, runtime.nativeHandle(), handle(), peerBytes(peer, arena)),
          fn);
    }
  }

  private static MemorySegment peerBytes(String peer, Arena arena) {
    return NativeConfig.bytesStruct(peer.getBytes(StandardCharsets.US_ASCII), arena);
  }

  private static void check(int status, String what) {
    if (status != 0) {
      throw IrohException.forStatus(
          IrohStatus.fromCode(status), what + " failed: " + NativeConfig.lastError());
    }
  }
}
