package site.aster.contract;

import static org.junit.jupiter.api.Assertions.assertEquals;

import java.util.List;
import java.util.Map;
import org.junit.jupiter.api.Test;
import site.aster.annotations.WireType;

/**
 * Cross-language conformance test for the Tarjan SCC + bottom-up hashing path.
 *
 * <p>The fixture is a three-node reference cycle Alpha → Beta → Gamma → Alpha. Every binding
 * (Python, Java, TS, Go, .NET) that mirrors this graph with the same {@code @WireType} tags and the
 * same field shapes must produce the same per-TypeDef hashes. Drift indicates a divergence in one
 * of: SCC ordering, back-edge classification, canonical byte emission, or BLAKE3 hashing.
 *
 * <p>The expected hashes come from {@code scripts/cross_lang_echo_contract_id.py} (Python is the
 * reference producer). To regenerate:
 *
 * <pre>
 *   uv run python scripts/cross_lang_echo_contract_id.py
 * </pre>
 *
 * <p>Why this graph: a 3-cycle is the smallest graph that exposes the {@code reversed(post_order)}
 * processing-order bug. With the bug, non-leaf SCC members emit zero-placeholder REFs for their
 * forward edges into the same SCC, so their hashes diverge from a correct implementation. All three
 * hashes below bake in real REF digests of their peers, so any binding that still has the bug will
 * fail at least two of the three assertions.
 *
 * <p>Requires {@code libaster_transport_ffi}.
 */
final class CrossLanguageCycleConformanceTest {

  // Reference hashes from the Python fixture. Keep in lockstep with
  // scripts/cross_lang_echo_contract_id.py — run it any time the model types change.
  private static final Map<String, String> EXPECTED_HASHES =
      Map.of(
          "chain.Alpha", "f2530f9d487afdce94b41eaa875b5aca0df1981de67185bb13796203492e2403",
          "chain.Beta", "5a7471f00be2437f28769074a809bae2daa4e15bafba59684e4bdebd10561f94",
          "chain.Gamma", "f6c956b80cc7015cb6cbafff68ab81faac06de1b50e6b9ab0610da46dd2094e9");

  @WireType("chain/Alpha")
  public record Alpha(String name, List<Beta> betas) {}

  @WireType("chain/Beta")
  public record Beta(String name, List<Gamma> gammas) {}

  @WireType("chain/Gamma")
  public record Gamma(String name, List<Alpha> alphas) {}

  @Test
  void threeCycleHashesMatchPythonReference() {
    var graph = TypeGraphWalker.walk(List.of(Alpha.class));
    var resolved = ContractIdentityResolver.resolve(graph);

    assertEquals(
        3,
        resolved.typeHashes().size(),
        "walker should discover exactly 3 types for the Alpha/Beta/Gamma cycle");

    for (var expected : EXPECTED_HASHES.entrySet()) {
      String actual = resolved.typeHashes().get(expected.getKey());
      assertEquals(
          expected.getValue(),
          actual,
          "type hash for "
              + expected.getKey()
              + " must match Python's reference. If the drift is intentional, rerun "
              + "scripts/cross_lang_echo_contract_id.py and update EXPECTED_HASHES.");
    }
  }

  @Test
  void allThreeHashesAreReal() {
    // Paranoia: the Python fixture's zero-placeholder bug (pre-fix) produced some real hashes
    // too, so "matches Python" only catches drift if Python itself is correct. Explicitly
    // asserting no zero hash guards against a regression on either side.
    var graph = TypeGraphWalker.walk(List.of(Alpha.class));
    var resolved = ContractIdentityResolver.resolve(graph);
    for (var entry : resolved.typeHashes().entrySet()) {
      assertEquals(
          64,
          entry.getValue().length(),
          entry.getKey() + " hash must be 64-char hex: " + entry.getValue());
      assert !"00".repeat(32).equals(entry.getValue())
          : entry.getKey() + " resolved to the zero-placeholder — SCC ordering regressed";
    }
  }
}
