package site.aster.audit;

import static org.junit.jupiter.api.Assertions.*;

import java.math.BigInteger;
import java.nio.charset.StandardCharsets;
import java.nio.file.Path;
import org.junit.jupiter.api.Test;

final class AuditOutboxModelTest {
  @Test
  void exactUnsignedTimestampAndDefensivePayload() {
    var payload = AuditJson.MAPPER.createObjectNode().put("count", 123);
    var timestamp = BigInteger.ONE.shiftLeft(64).subtract(BigInteger.ONE);
    var event = new AuditOutbox.Event(timestamp, "intent", payload);
    payload.put("count", 456);
    var json = new String(AuditJson.request("event", event), StandardCharsets.UTF_8);
    assertTrue(json.contains("18446744073709551615"));
    assertTrue(json.contains("\"count\":123"));
    assertFalse(event.toString().contains("123"));
    ((com.fasterxml.jackson.databind.node.ObjectNode) event.payload()).put("count", 789);
    assertEquals(123, event.payload().path("count").asInt());
    assertThrows(
        IllegalArgumentException.class, () -> new AuditOutbox.Event(-1, "invalid", payload));
  }

  @Test
  void CredentialsSerializeOnlyInExplicitConfigurationAndDoNotPrint() {
    var destination =
        new AuditOutbox.Destination(
            "https://receiver.test/audit?private=secret", "receiver-1", "Bearer credential");
    var config = new AuditOutbox.Config(Path.of("state/audit"), destination);
    var json = new String(AuditJson.config(config), StandardCharsets.UTF_8);
    assertTrue(json.contains("Bearer credential"));
    assertTrue(json.contains("receiver_identity"));
    assertFalse(destination.toString().contains("credential"));
    assertFalse(destination.toString().contains("secret"));
    assertFalse(config.toString().contains("credential"));
    assertEquals(BigInteger.valueOf(256L * 1024 * 1024), config.maxRetainedBytes());
  }
}
