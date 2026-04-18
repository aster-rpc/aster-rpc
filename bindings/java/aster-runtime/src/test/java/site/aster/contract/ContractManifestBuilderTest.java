package site.aster.contract;

import static org.junit.jupiter.api.Assertions.assertEquals;
import static org.junit.jupiter.api.Assertions.assertFalse;
import static org.junit.jupiter.api.Assertions.assertNotEquals;
import static org.junit.jupiter.api.Assertions.assertNotNull;
import static org.junit.jupiter.api.Assertions.assertTrue;

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
 * End-to-end tests for {@link ContractManifestBuilder}. Uses hand-written {@link
 * ServiceDispatcher}s so the test does not depend on the codegen path — the manifest pipeline is
 * what's under test.
 *
 * <p>Requires {@code libaster_transport_ffi} to be loadable for {@code aster_contract_id} and
 * {@code aster_blake3_hex} calls.
 */
final class ContractManifestBuilderTest {

  @WireType("echo/EchoRequest")
  public record EchoRequest(String message) {}

  @WireType("echo/EchoResponse")
  public record EchoResponse(String reply) {}

  /**
   * Minimal EchoService dispatcher. Only unary.echo is populated; no interceptors, no metadata.
   * Non-final so subclasses in later tests can override metadata accessors.
   */
  static class EchoDispatcher implements ServiceDispatcher {
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
            true);

    @Override
    public MethodDescriptor descriptor() {
      return DESCRIPTOR;
    }

    @Override
    public byte[] invoke(Object impl, byte[] requestBytes, Codec codec, CallContext ctx) {
      return new byte[0];
    }
  }

  // ── Basic shape ───────────────────────────────────────────────────────────

  @Test
  void echoServiceProducesFullManifest() {
    ContractManifest m = ContractManifestBuilder.build(new EchoDispatcher());

    assertEquals(ContractManifest.FIELD_SCHEMA_VERSION, m.v());
    assertEquals("EchoService", m.service());
    assertEquals(1, m.version());
    assertEquals("fory-xlang/0.15", m.canonicalEncoding());
    assertEquals("shared", m.scoped());
    assertEquals(List.of("xlang"), m.serializationModes());

    // contract_id is a real BLAKE3 hex, not zeros.
    assertTrue(m.contractId().matches("[0-9a-f]{64}"));
    assertNotEquals("00".repeat(32), m.contractId());

    // Type graph: Req + Resp (two records; no nested user types).
    assertEquals(2, m.typeCount());
    assertEquals(2, m.typeHashes().size());
    for (String h : m.typeHashes()) {
      assertTrue(h.matches("[0-9a-f]{64}"), "bad type hash: " + h);
      assertNotEquals("00".repeat(32), h);
    }

    assertEquals(1, m.methodCount());
    Map<String, Object> method = m.methods().get(0);
    assertEquals("echo", method.get("name"));
    assertEquals("unary", method.get("pattern"));
    assertEquals("EchoRequest", method.get("request_type"));
    assertEquals("EchoResponse", method.get("response_type"));
    assertEquals("echo/EchoRequest", method.get("request_wire_tag"));
    assertEquals("echo/EchoResponse", method.get("response_wire_tag"));
    assertEquals(true, method.get("idempotent"));
    assertEquals(false, method.get("has_context_param"));
    assertEquals("explicit", method.get("request_style"));

    @SuppressWarnings("unchecked")
    List<Map<String, Object>> fields = (List<Map<String, Object>>) method.get("fields");
    assertEquals(1, fields.size());
    assertEquals("message", fields.get(0).get("name"));
    assertEquals("string", fields.get(0).get("kind"));

    @SuppressWarnings("unchecked")
    List<Map<String, Object>> respFields =
        (List<Map<String, Object>>) method.get("response_fields");
    assertEquals(1, respFields.size());
    assertEquals("reply", respFields.get(0).get("name"));
  }

  @Test
  void manifestJsonIsValidAndRoundTrips() {
    ContractManifest m = ContractManifestBuilder.build(new EchoDispatcher());
    String json = m.toJson();
    assertNotNull(json);
    assertTrue(json.contains("\"service\":\"EchoService\""));
    assertTrue(json.contains("\"contract_id\":\"" + m.contractId() + "\""));
    // v1 keys (a sampling).
    assertTrue(json.contains("\"canonical_encoding\""));
    assertTrue(json.contains("\"type_hashes\""));
    assertTrue(json.contains("\"producer_language\""));

    ContractManifest round = ContractJson.fromJson(json, ContractManifest.class);
    assertEquals(m.service(), round.service());
    assertEquals(m.contractId(), round.contractId());
    assertEquals(m.typeHashes(), round.typeHashes());
    assertEquals(m.methodCount(), round.methodCount());
  }

  @Test
  void contractIdMatchesStandaloneComputeCall() {
    EchoDispatcher d = new EchoDispatcher();
    assertEquals(
        ContractManifestBuilder.build(d).contractId(),
        ContractManifestBuilder.computeContractId(d));
  }

  // ── Determinism ───────────────────────────────────────────────────────────

  @Test
  void contractIdIsStableAcrossBuilds() {
    String a = ContractManifestBuilder.computeContractId(new EchoDispatcher());
    String b = ContractManifestBuilder.computeContractId(new EchoDispatcher());
    assertEquals(a, b);
  }

  @Test
  void renamingServiceChangesContractId() {
    EchoDispatcher base = new EchoDispatcher();
    ServiceDispatcher renamed =
        new EchoDispatcher() {
          @Override
          public ServiceDescriptor descriptor() {
            return new ServiceDescriptor("DifferentService", 1, Scope.SHARED, Object.class);
          }
        };
    assertNotEquals(
        ContractManifestBuilder.computeContractId(base),
        ContractManifestBuilder.computeContractId(renamed));
  }

  // ── Rich metadata flows through ──────────────────────────────────────────

  @Test
  void dispatcherMetadataSurfacesOnTheManifest() {
    ServiceDispatcher withMeta =
        new EchoDispatcher() {
          @Override
          public String description() {
            return "Echoes whatever you send.";
          }

          @Override
          public List<String> tags() {
            return List.of("readonly", "diagnostic");
          }
        };
    ContractManifest m = ContractManifestBuilder.build(withMeta);
    assertEquals("Echoes whatever you send.", m.description());
    assertEquals(List.of("readonly", "diagnostic"), m.tags());
    assertFalse(m.deprecated());
  }
}
