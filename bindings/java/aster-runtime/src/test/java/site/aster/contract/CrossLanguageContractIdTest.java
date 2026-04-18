package site.aster.contract;

import static org.junit.jupiter.api.Assertions.assertEquals;

import java.util.List;
import java.util.Map;
import org.apache.fory.Fory;
import org.junit.jupiter.api.Test;
import site.aster.annotations.Scope;
import site.aster.annotations.WireType;
import site.aster.codec.Codec;
import site.aster.interceptors.CallContext;
import site.aster.server.spi.MethodDescriptor;
import site.aster.server.spi.MethodDispatcher;
import site.aster.server.spi.RequestStyle;
import site.aster.server.spi.ServiceDescriptor;
import site.aster.server.spi.ServiceDispatcher;
import site.aster.server.spi.StreamingKind;
import site.aster.server.spi.UnaryDispatcher;

/**
 * The acid test. A Python {@code EchoService} with the same wire identity (via {@code @wire_type})
 * as the Java fixture below must produce an identical {@code contract_id} — byte-identical
 * canonical bytes running through the same Rust canonicalizer.
 *
 * <p>The Python reference lives at {@code scripts/cross_lang_echo_contract_id.py} and the hash it
 * produces is hardcoded here. Regenerating either side without the other must break this test so
 * drift is caught at CI time. Requires {@code libaster_transport_ffi} to be loadable.
 *
 * <p>Test vector regeneration (run from repo root):
 *
 * <pre>
 *   uv run python scripts/cross_lang_echo_contract_id.py
 * </pre>
 */
final class CrossLanguageContractIdTest {

  /**
   * Value captured from {@code scripts/cross_lang_echo_contract_id.py} (Python reference). If this
   * test fails and the diff is intentional, regenerate the Python side and update this constant.
   */
  private static final String PYTHON_ECHO_CONTRACT_ID =
      "12d2f2990f4dd71dfd59f5db470d186f1fcc7dbafdac0ea7fdf838ab263c0578";

  // Same @WireType tags as Python's @wire_type — this is the one knob that controls FQN parity.
  @WireType("echo/EchoRequest")
  public record EchoRequest(String message) {}

  @WireType("echo/EchoResponse")
  public record EchoResponse(String reply) {}

  @Test
  void javaEchoServiceMatchesPythonContractId() {
    String javaId = ContractManifestBuilder.computeContractId(new EchoDispatcher());
    assertEquals(
        PYTHON_ECHO_CONTRACT_ID,
        javaId,
        "Java contract_id must match Python's for the same logical EchoService. "
            + "If this is intentional, rerun scripts/cross_lang_echo_contract_id.py and update "
            + "PYTHON_ECHO_CONTRACT_ID.");
  }

  // ── Fixture ───────────────────────────────────────────────────────────────

  static final class EchoDispatcher implements ServiceDispatcher {
    static final ServiceDescriptor DESCRIPTOR =
        new ServiceDescriptor("EchoService", 1, Scope.SHARED, Object.class);
    static final Map<String, MethodDispatcher> METHODS = Map.of("echo", new EchoMethod());
    static final Map<String, Class<?>> REQ = Map.of("echo", EchoRequest.class);
    static final Map<String, Class<?>> RESP = Map.of("echo", EchoResponse.class);

    @Override
    public ServiceDescriptor descriptor() {
      return DESCRIPTOR;
    }

    @Override
    public Map<String, MethodDispatcher> methods() {
      return METHODS;
    }

    @Override
    public void registerTypes(Fory fory) {}

    @Override
    public Map<String, Class<?>> requestClasses() {
      return REQ;
    }

    @Override
    public Map<String, Class<?>> responseClasses() {
      return RESP;
    }
  }

  static final class EchoMethod implements UnaryDispatcher {
    static final MethodDescriptor DESCRIPTOR =
        new MethodDescriptor(
            "echo",
            StreamingKind.UNARY,
            RequestStyle.EXPLICIT,
            "echo/EchoRequest",
            List.of(),
            "echo/EchoResponse",
            false,
            false);

    @Override
    public MethodDescriptor descriptor() {
      return DESCRIPTOR;
    }

    @Override
    public byte[] invoke(Object impl, byte[] requestBytes, Codec codec, CallContext ctx) {
      return new byte[0];
    }
  }
}
