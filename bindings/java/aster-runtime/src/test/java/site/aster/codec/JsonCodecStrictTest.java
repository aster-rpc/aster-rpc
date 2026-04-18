package site.aster.codec;

import static org.junit.jupiter.api.Assertions.assertEquals;
import static org.junit.jupiter.api.Assertions.assertInstanceOf;
import static org.junit.jupiter.api.Assertions.assertThrows;
import static org.junit.jupiter.api.Assertions.assertTrue;

import com.fasterxml.jackson.annotation.JsonCreator;
import com.fasterxml.jackson.annotation.JsonProperty;
import java.util.ArrayList;
import java.util.List;
import org.junit.jupiter.api.Test;
import site.aster.interceptors.ContractViolationError;
import site.aster.interceptors.StatusCode;

/**
 * Strict-mode tests for {@link JsonCodec}. Mirrors {@code tests/python/test_codec_strict.py}: any
 * JSON key that doesn't map to a declared field on the expected class raises {@link
 * ContractViolationError} at any nesting depth. The codec never silently drops or renames keys.
 */
final class JsonCodecStrictTest {

  // ── Fixtures ───────────────────────────────────────────────────────────────

  public static final class StatusRequest {
    public String agentId = "";
    public String region = "";
  }

  public static final class Tag {
    public String key = "";
    public String value = "";
  }

  public static final class StatusResponse {
    public String agentId = "";
    public String status = "";
    public List<Tag> tags = new ArrayList<>();
    public Tag nested = new Tag();
  }

  // Record-based variant to pin strict behavior on immutable types too.
  public record RecordRequest(String agentId, String region) {
    @JsonCreator
    public RecordRequest(
        @JsonProperty("agentId") String agentId, @JsonProperty("region") String region) {
      this.agentId = agentId == null ? "" : agentId;
      this.region = region == null ? "" : region;
    }
  }

  // ── Top-level violations ───────────────────────────────────────────────────

  @Test
  void unexpectedTopLevelKeyRaises() {
    JsonCodec codec = new JsonCodec();
    byte[] payload = "{\"agentId\":\"ok\",\"bogus\":1}".getBytes();

    ContractViolationError err =
        assertThrows(
            ContractViolationError.class, () -> codec.decode(payload, StatusRequest.class));

    assertEquals(StatusCode.CONTRACT_VIOLATION, err.code());
    assertTrue(err.rpcMessage().contains("bogus"), err.rpcMessage());
    assertTrue(err.rpcMessage().contains("StatusRequest"), err.rpcMessage());
    assertEquals("StatusRequest", err.details().get("expected_class"));
    assertTrue(err.details().get("unexpected_fields").contains("bogus"));
  }

  @Test
  void validRequestDecodesCleanly() {
    JsonCodec codec = new JsonCodec();
    byte[] payload = "{\"agentId\":\"edge-7\",\"region\":\"us-east\"}".getBytes();
    StatusRequest req = (StatusRequest) codec.decode(payload, StatusRequest.class);
    assertEquals("edge-7", req.agentId);
    assertEquals("us-east", req.region);
  }

  @Test
  void missingFieldUsesDefault() {
    JsonCodec codec = new JsonCodec();
    byte[] payload = "{\"agentId\":\"edge-7\"}".getBytes();
    StatusRequest req = (StatusRequest) codec.decode(payload, StatusRequest.class);
    assertEquals("edge-7", req.agentId);
    assertEquals("", req.region);
  }

  @Test
  void recordRoundTripsAndRejectsUnknown() {
    JsonCodec codec = new JsonCodec();
    byte[] ok = "{\"agentId\":\"x\",\"region\":\"y\"}".getBytes();
    RecordRequest r = (RecordRequest) codec.decode(ok, RecordRequest.class);
    assertEquals("x", r.agentId());
    assertEquals("y", r.region());

    byte[] bad = "{\"agentId\":\"x\",\"region\":\"y\",\"nope\":1}".getBytes();
    assertThrows(ContractViolationError.class, () -> codec.decode(bad, RecordRequest.class));
  }

  // ── Nested violations ──────────────────────────────────────────────────────

  @Test
  void unexpectedNestedObjectKeyRaisesWithPath() {
    JsonCodec codec = new JsonCodec();
    String json =
        "{"
            + "\"agentId\":\"ok\","
            + "\"status\":\"running\","
            + "\"tags\":[],"
            + "\"nested\":{\"key\":\"k\",\"value\":\"v\",\"rogue\":1}"
            + "}";

    ContractViolationError err =
        assertThrows(
            ContractViolationError.class,
            () -> codec.decode(json.getBytes(), StatusResponse.class));

    String location = err.details().get("location");
    assertTrue(
        location.startsWith("StatusResponse") && location.contains("nested"),
        "location should point to nested path: " + location);
    assertTrue(err.details().get("unexpected_fields").contains("rogue"));
  }

  @Test
  void unexpectedArrayElementKeyRaisesWithIndex() {
    JsonCodec codec = new JsonCodec();
    String json =
        "{"
            + "\"agentId\":\"ok\","
            + "\"status\":\"running\","
            + "\"tags\":[{\"key\":\"a\",\"value\":\"1\"},{\"key\":\"b\",\"value\":\"2\",\"snuckIn\":true}],"
            + "\"nested\":{\"key\":\"k\",\"value\":\"v\"}"
            + "}";

    ContractViolationError err =
        assertThrows(
            ContractViolationError.class,
            () -> codec.decode(json.getBytes(), StatusResponse.class));

    String location = err.details().get("location");
    assertTrue(
        location.contains("tags") && location.contains("[1]"),
        "location should include array index: " + location);
    assertTrue(err.details().get("unexpected_fields").contains("snuckIn"));
  }

  // ── Permissive path ────────────────────────────────────────────────────────

  @Test
  void nullOrObjectTypeDecodesPermissively() {
    JsonCodec codec = new JsonCodec();
    byte[] payload = "{\"anything\":\"goes\",\"nested\":{\"deep\":1}}".getBytes();
    Object untyped = codec.decode(payload, null);
    assertInstanceOf(java.util.Map.class, untyped);
    Object untyped2 = codec.decode(payload, Object.class);
    assertInstanceOf(java.util.Map.class, untyped2);
  }

  // ── Round trip ─────────────────────────────────────────────────────────────

  @Test
  void encodeAndDecodeRoundTrip() {
    JsonCodec codec = new JsonCodec();
    StatusRequest r = new StatusRequest();
    r.agentId = "a";
    r.region = "b";
    byte[] bytes = codec.encode(r);
    StatusRequest back = (StatusRequest) codec.decode(bytes, StatusRequest.class);
    assertEquals("a", back.agentId);
    assertEquals("b", back.region);
  }

  @Test
  void emptyPayloadDecodesToNull() {
    JsonCodec codec = new JsonCodec();
    assertEquals(null, codec.decode(new byte[0], StatusRequest.class));
    assertEquals(null, codec.decode(null, StatusRequest.class));
  }

  @Test
  void nullValueEncodesToEmptyBytes() {
    JsonCodec codec = new JsonCodec();
    byte[] bytes = codec.encode(null);
    assertEquals(0, bytes.length);
  }

  @Test
  void modeIsJson() {
    assertEquals("json", new JsonCodec().mode());
  }

  // ── Sanitization ───────────────────────────────────────────────────────────

  @Test
  void sanitizeCapsKeyCount() {
    List<String> many = List.of("a", "b", "c", "d", "e", "f", "g");
    List<String> out = JsonCodec.sanitizeKeys(many);
    assertEquals(6, out.size(), "5 keys + a '+N more' marker");
    assertTrue(out.get(5).contains("+2 more"), out.get(5));
  }

  @Test
  void sanitizeTruncatesLongKeys() {
    String longKey = "x".repeat(200);
    List<String> out = JsonCodec.sanitizeKeys(List.of(longKey));
    assertTrue(out.get(0).contains("...(truncated)"), out.get(0));
    assertTrue(out.get(0).length() < 120, out.get(0));
  }

  @Test
  void sanitizeEscapesControlChars() {
    List<String> out = JsonCodec.sanitizeKeys(List.of("evil\nkey\twith\rctrl"));
    String v = out.get(0);
    assertTrue(v.contains("\\n") && v.contains("\\t") && v.contains("\\r"), v);
    assertTrue(!v.contains("\n") && !v.contains("\r"), "raw control chars escaped: " + v);
  }
}
