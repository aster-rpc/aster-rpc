package site.aster.contract;

import java.lang.foreign.Arena;
import java.lang.foreign.MemorySegment;
import java.lang.foreign.ValueLayout;
import java.nio.charset.StandardCharsets;
import site.aster.ffi.IrohLibrary;

/**
 * Computes Aster contract identities via the Rust FFI canonicalizer.
 *
 * <p>All canonicalization and BLAKE3 hashing is done in Rust -- Java never computes canonical bytes
 * or hashes locally. This follows the universal decodability axiom rule in spec section 11.3.2.3:
 * "Bindings MUST NOT implement canonicalization or BLAKE3 hashing in their own language."
 *
 * <p>Usage:
 *
 * <pre>{@code
 * String contractJson = """
 *     {"name": "HelloService", "version": 1, "methods": [],
 *      "serialization_modes": ["xlang"], "scoped": "shared",
 *      "requires": null, "producer_language": ""}
 *     """;
 * String contractId = ContractIdentity.computeContractId(contractJson);
 * // contractId is a 64-char hex BLAKE3 hash
 * }</pre>
 */
public final class ContractIdentity {

  /** Maximum expected output size for a 64-char hex contract_id. */
  private static final int CONTRACT_ID_BUF_SIZE = 128;

  private ContractIdentity() {}

  /**
   * Compute the contract_id from a ServiceContract JSON string.
   *
   * <p>The JSON must match the serde shape of {@code core::contract::ServiceContract} (fields:
   * name, version, methods, serialization_modes, scoped, requires, producer_language). The Rust FFI
   * validates the producer_language invariant (must be empty unless "native" in
   * serialization_modes).
   *
   * @param serviceContractJson UTF-8 JSON string describing the ServiceContract
   * @return 64-character hex-encoded BLAKE3 contract hash
   * @throws IllegalArgumentException if the JSON is invalid or violates invariants
   */
  public static String computeContractId(String serviceContractJson) {
    IrohLibrary lib = IrohLibrary.getInstance();
    byte[] jsonBytes = serviceContractJson.getBytes(StandardCharsets.UTF_8);

    try (Arena arena = Arena.ofConfined()) {
      // Copy JSON into native memory
      MemorySegment jsonSeg = arena.allocate(jsonBytes.length);
      jsonSeg.copyFrom(MemorySegment.ofArray(jsonBytes));

      // Allocate output buffer + length pointer
      MemorySegment outBuf = arena.allocate(CONTRACT_ID_BUF_SIZE);
      MemorySegment outLen = arena.allocate(ValueLayout.JAVA_LONG);
      outLen.set(ValueLayout.JAVA_LONG, 0, CONTRACT_ID_BUF_SIZE);

      int status = lib.asterContractId(jsonSeg, jsonBytes.length, outBuf, outLen);
      if (status != 0) {
        throw new IllegalArgumentException(
            "aster_contract_id failed with status "
                + status
                + ". Check the ServiceContract JSON for validity.");
      }

      long written = outLen.get(ValueLayout.JAVA_LONG, 0);
      byte[] result = new byte[(int) written];
      MemorySegment.copy(outBuf, ValueLayout.JAVA_BYTE, 0, result, 0, (int) written);
      return new String(result, StandardCharsets.UTF_8);
    }
  }

  /**
   * Compute canonical bytes for a named type from JSON.
   *
   * @param typeName one of "ServiceContract", "TypeDef", "MethodDef"
   * @param json UTF-8 JSON string describing the type
   * @return canonical XLANG bytes
   * @throws IllegalArgumentException if the JSON is invalid
   */
  public static byte[] computeCanonicalBytes(String typeName, String json) {
    IrohLibrary lib = IrohLibrary.getInstance();
    byte[] typeNameBytes = typeName.getBytes(StandardCharsets.UTF_8);
    byte[] jsonBytes = json.getBytes(StandardCharsets.UTF_8);

    try (Arena arena = Arena.ofConfined()) {
      MemorySegment typeNameSeg = arena.allocate(typeNameBytes.length);
      typeNameSeg.copyFrom(MemorySegment.ofArray(typeNameBytes));

      MemorySegment jsonSeg = arena.allocate(jsonBytes.length);
      jsonSeg.copyFrom(MemorySegment.ofArray(jsonBytes));

      // Start with a reasonable buffer; retry on BUFFER_TOO_SMALL
      int bufSize = 4096;
      MemorySegment outBuf = arena.allocate(bufSize);
      MemorySegment outLen = arena.allocate(ValueLayout.JAVA_LONG);
      outLen.set(ValueLayout.JAVA_LONG, 0, bufSize);

      int status =
          lib.asterCanonicalBytes(
              typeNameSeg, typeNameBytes.length, jsonSeg, jsonBytes.length, outBuf, outLen);

      if (status != 0) {
        throw new IllegalArgumentException("aster_canonical_bytes failed with status " + status);
      }

      long written = outLen.get(ValueLayout.JAVA_LONG, 0);
      byte[] result = new byte[(int) written];
      MemorySegment.copy(outBuf, ValueLayout.JAVA_BYTE, 0, result, 0, (int) written);
      return result;
    }
  }
}
