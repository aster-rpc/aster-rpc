package site.aster.ffi;

import static org.junit.jupiter.api.Assertions.assertArrayEquals;
import static org.junit.jupiter.api.Assertions.assertEquals;
import static org.junit.jupiter.api.Assertions.assertNotNull;
import static org.junit.jupiter.api.Assertions.assertNull;
import static org.junit.jupiter.api.Assertions.assertThrows;
import static org.junit.jupiter.api.Assertions.assertTrue;

import com.fasterxml.jackson.databind.JsonNode;
import com.fasterxml.jackson.databind.ObjectMapper;
import java.io.IOException;
import java.lang.foreign.MemoryLayout;
import java.nio.ByteBuffer;
import java.nio.ByteOrder;
import java.nio.file.Files;
import java.nio.file.Path;
import java.util.ArrayList;
import java.util.Arrays;
import java.util.HexFormat;
import java.util.List;
import org.junit.jupiter.api.Test;
import site.aster.server.AsterFraming;
import site.aster.server.wire.CallHeader;
import site.aster.server.wire.RpcStatus;
import site.aster.server.wire.StreamHeader;

/**
 * {@link AsterEnvelope} (C ABI 3.1 envelope + zstd functions) against the shared conformance
 * vectors: {@code conformance/vectors/envelope.json} and {@code
 * conformance/vectors/compression.json}.
 */
final class AsterEnvelopeTest {

  private static final HexFormat HEX = HexFormat.of();
  private static final ObjectMapper JSON = new ObjectMapper();

  /** Walk up from the working directory to the repo's {@code conformance/vectors/<name>}. */
  static JsonNode vectors(String name) throws IOException {
    Path dir = Path.of("").toAbsolutePath();
    for (int i = 0; i < 8 && dir != null; i++, dir = dir.getParent()) {
      Path p = dir.resolve("conformance").resolve("vectors").resolve(name);
      if (Files.exists(p)) {
        return JSON.readTree(p.toFile());
      }
    }
    throw new IllegalStateException("conformance/vectors/" + name + " not found above cwd");
  }

  private static List<String> strings(JsonNode arr) {
    List<String> out = new ArrayList<>();
    arr.forEach(n -> out.add(n.asText()));
    return out;
  }

  // ─── layout ────────────────────────────────────────────────────────────────

  private static long off(MemoryLayout l, String f) {
    return l.byteOffset(MemoryLayout.PathElement.groupElement(f));
  }

  @Test
  void layoutsMatchHeader() {
    assertTrue(IrohLibrary.getInstance().abiVersionMinor() >= AsterEnvelope.REQUIRED_ABI_MINOR);
    assertEquals(96, AsterEnvelope.STREAM_HEADER.byteSize());
    assertEquals(16, off(AsterEnvelope.STREAM_HEADER, "deadline"));
    assertEquals(18, off(AsterEnvelope.STREAM_HEADER, "serialization_mode"));
    assertEquals(24, off(AsterEnvelope.STREAM_HEADER, "service"));
    assertEquals(56, off(AsterEnvelope.STREAM_HEADER, "metadata_keys"));
    assertEquals(88, off(AsterEnvelope.STREAM_HEADER, "storage"));
    assertEquals(72, AsterEnvelope.CALL_HEADER.byteSize());
    assertEquals(16, off(AsterEnvelope.CALL_HEADER, "method"));
    assertEquals(64, off(AsterEnvelope.CALL_HEADER, "storage"));
    assertEquals(64, AsterEnvelope.RPC_STATUS.byteSize());
    assertEquals(8, off(AsterEnvelope.RPC_STATUS, "message"));
    assertEquals(56, off(AsterEnvelope.RPC_STATUS, "storage"));
  }

  // ─── envelope.json ─────────────────────────────────────────────────────────

  @Test
  void envelopeVectorsEncodeExactlyAndDecodeBack() throws IOException {
    JsonNode doc = vectors("envelope.json");
    int n = 0;
    for (JsonNode v : doc.get("vectors")) {
      byte[] want = HEX.parseHex(v.get("hex").asText());
      JsonNode in = v.get("input");
      String name = v.get("name").asText();
      switch (v.get("type").asText()) {
        case "StreamHeader" -> {
          StreamHeader h =
              new StreamHeader(
                  in.get("service").asText(),
                  in.get("method").asText(),
                  in.get("version").asInt(),
                  in.get("callId").asInt(),
                  (short) in.get("deadline").asInt(),
                  (byte) in.get("serializationMode").asInt(),
                  strings(in.get("metadataKeys")),
                  strings(in.get("metadataValues")),
                  in.get("sessionId").asInt());
          assertArrayEquals(want, AsterEnvelope.encodeStreamHeader(h), name);
          assertEquals(h, AsterEnvelope.decodeStreamHeader(want), name);
        }
        case "CallHeader" -> {
          CallHeader h =
              new CallHeader(
                  in.get("method").asText(),
                  in.get("callId").asInt(),
                  (short) in.get("deadline").asInt(),
                  strings(in.get("metadataKeys")),
                  strings(in.get("metadataValues")));
          assertArrayEquals(want, AsterEnvelope.encodeCallHeader(h), name);
          assertEquals(h, AsterEnvelope.decodeCallHeader(want), name);
        }
        case "RpcStatus" -> {
          RpcStatus s =
              new RpcStatus(
                  in.get("code").asInt(),
                  in.get("message").asText(),
                  strings(in.get("detailKeys")),
                  strings(in.get("detailValues")));
          assertArrayEquals(want, AsterEnvelope.encodeRpcStatus(s), name);
          assertEquals(s, AsterEnvelope.decodeRpcStatus(want), name);
        }
        default -> throw new AssertionError("unknown vector type in " + name);
      }
      n++;
    }
    assertTrue(n >= 10, "expected at least 10 envelope vectors, got " + n);
  }

  @Test
  void envelopeInvalidVectorsAreRefused() throws IOException {
    JsonNode doc = vectors("envelope.json");
    for (JsonNode v : doc.get("invalid")) {
      byte[] bytes = HEX.parseHex(v.get("hex").asText());
      String name = v.get("name").asText();
      IllegalArgumentException e =
          assertThrows(
              IllegalArgumentException.class,
              () -> {
                switch (v.get("type").asText()) {
                  case "StreamHeader" -> AsterEnvelope.decodeStreamHeader(bytes);
                  case "CallHeader" -> AsterEnvelope.decodeCallHeader(bytes);
                  case "RpcStatus" -> AsterEnvelope.decodeRpcStatus(bytes);
                  default -> throw new AssertionError("unknown type");
                }
              },
              name);
      assertTrue(e.getMessage().contains("envelope"), name + ": " + e.getMessage());
    }
  }

  @Test
  void encodeRefusesKeyValueMismatch() {
    RpcStatus bad = new RpcStatus(0, "", List.of("k"), List.of());
    assertThrows(IllegalArgumentException.class, () -> AsterEnvelope.encodeRpcStatus(bad));
  }

  @Test
  void tagsDistinguishEnvelopeTypes() {
    byte[] sh = AsterEnvelope.encodeStreamHeader(new StreamHeader());
    assertEquals(AsterEnvelope.TAG_STREAM_HEADER, sh[0]);
    assertEquals(AsterEnvelope.TAG_RPC_STATUS, AsterEnvelope.encodeRpcStatus(RpcStatus.ok())[0]);
    assertThrows(IllegalArgumentException.class, () -> AsterEnvelope.decodeRpcStatus(sh));
    assertThrows(IllegalArgumentException.class, () -> AsterEnvelope.decodeCallHeader(sh));
  }

  // ─── compression.json ──────────────────────────────────────────────────────

  private static byte[] payload(String rule, int len) {
    byte[] out = new byte[len];
    switch (rule) {
      case "byte[i] = i % 251" -> {
        for (int i = 0; i < len; i++) out[i] = (byte) (i % 251);
      }
      case "byte[i] = 0" -> {}
      default -> throw new AssertionError("unknown payload_rule " + rule);
    }
    return out;
  }

  /** Whole frame {@code u32 LE length | flags | payload} → [flags], payload. */
  private static byte[] frameBody(byte[] frame, byte[] flagsOut) {
    int n = ByteBuffer.wrap(frame, 0, 4).order(ByteOrder.LITTLE_ENDIAN).getInt();
    assertEquals(frame.length, n + 4);
    flagsOut[0] = frame[4];
    return Arrays.copyOfRange(frame, 5, frame.length);
  }

  @Test
  void compressionDecodeVectors() throws IOException {
    for (JsonNode v : vectors("compression.json").get("decode_vectors")) {
      byte[] flags = new byte[1];
      byte[] body = frameBody(HEX.parseHex(v.get("frame_hex").asText()), flags);
      assertEquals(v.get("flags").asInt(), flags[0] & 0xff);
      byte[] got = AsterFraming.payloadOf(body, flags[0]);
      int len = v.get("payload_length").asInt();
      assertArrayEquals(payload(v.get("payload_rule").asText(), len), got, v.get("name").asText());
    }
  }

  @Test
  void compressionRejectVectors() throws IOException {
    for (JsonNode v : vectors("compression.json").get("reject_vectors")) {
      byte[] flags = new byte[1];
      byte[] body = frameBody(HEX.parseHex(v.get("frame_hex").asText()), flags);
      assertThrows(
          IllegalArgumentException.class,
          () -> AsterEnvelope.decompress(body),
          v.get("name").asText());
    }
  }

  @Test
  void compressionEncodeFollowsThresholdRule() throws IOException {
    JsonNode doc = vectors("compression.json");
    int threshold = doc.get("parameters").get("threshold_bytes").asInt();
    assertEquals(AsterEnvelope.COMPRESSION_THRESHOLD, threshold);
    assertEquals(
        AsterEnvelope.MAX_DECOMPRESSED_SIZE,
        doc.get("parameters").get("max_decompressed_bytes").asInt());
    for (JsonNode v : doc.get("encode_vectors")) {
      String name = v.get("name").asText();
      byte[] p = payload(v.get("payload_rule").asText(), v.get("payload_length").asInt());
      byte[] wantFlags = new byte[1];
      frameBody(HEX.parseHex(v.get("expected_frame_hex").asText()), wantFlags);
      boolean wantCompressed = (wantFlags[0] & AsterFraming.FLAG_COMPRESSED) != 0;

      byte[] c = AsterEnvelope.maybeCompress(p, threshold);
      assertEquals(wantCompressed, c != null, name);
      // Framing helper: same decision, and the receiver recovers the payload.
      byte[] gotFlags = new byte[1];
      byte[] body = frameBody(AsterFraming.encodePayloadFrame(p, (byte) 0), gotFlags);
      assertEquals(wantCompressed, (gotFlags[0] & AsterFraming.FLAG_COMPRESSED) != 0, name);
      assertArrayEquals(p, AsterFraming.payloadOf(body, gotFlags[0]), name);
      if (c != null) {
        assertTrue(c.length < p.length, name);
        assertArrayEquals(p, AsterEnvelope.decompress(c), name);
      }
    }
    assertNull(AsterEnvelope.maybeCompress(new byte[0], threshold));
    assertNotNull(AsterEnvelope.maybeCompress(new byte[threshold + 1], threshold));
  }
}
