package site.aster.client;

import static org.junit.jupiter.api.Assertions.assertEquals;

import java.util.List;
import org.junit.jupiter.api.Test;
import site.aster.codec.ForyCodec;
import site.aster.server.wire.CallHeader;
import site.aster.server.wire.RpcStatus;
import site.aster.server.wire.StreamHeader;

/**
 * Proves {@link StreamHeader}, {@link CallHeader}, and {@link RpcStatus} round-trip across two
 * independent Fory xlang instances in the same JVM. This is the regression guard for the
 * class-vs-record wire-type bug uncovered in G.1: records carrying {@code List<String>} fields fail
 * to decode on a fresh Fory instance (even with identical registrations), which broke the very
 * first Java-to-Java unary test. The three wire types are plain classes for exactly this reason; if
 * they ever get migrated back to records, this test will fail first.
 */
final class StreamHeaderRoundTripTest {

  private static ForyCodec registerAll(ForyCodec c) {
    c.fory().register(StreamHeader.class, "_aster/StreamHeader");
    c.fory().register(CallHeader.class, "_aster/CallHeader");
    c.fory().register(RpcStatus.class, "_aster/RpcStatus");
    return c;
  }

  @Test
  void streamHeaderRoundTripsAcrossForyInstances() {
    ForyCodec writer = registerAll(new ForyCodec());
    ForyCodec reader = registerAll(new ForyCodec());

    StreamHeader header =
        new StreamHeader(
            "EchoService",
            "echo",
            1,
            0,
            (short) 0,
            StreamHeader.SERIALIZATION_XLANG,
            List.of(),
            List.of());

    byte[] encoded = writer.encode(header);
    StreamHeader decoded = (StreamHeader) reader.decode(encoded, StreamHeader.class);

    assertEquals(header.service(), decoded.service());
    assertEquals(header.method(), decoded.method());
    assertEquals(header.version(), decoded.version());
    assertEquals(header.callId(), decoded.callId());
    assertEquals(header.deadline(), decoded.deadline());
    assertEquals(header.serializationMode(), decoded.serializationMode());
  }

  @Test
  void rpcStatusOkRoundTrips() {
    ForyCodec writer = registerAll(new ForyCodec());
    ForyCodec reader = registerAll(new ForyCodec());

    byte[] encoded = writer.encode(RpcStatus.ok());
    RpcStatus decoded = (RpcStatus) reader.decode(encoded, RpcStatus.class);
    assertEquals(RpcStatus.OK, decoded.code());
    assertEquals("", decoded.message());
  }

  @Test
  void callHeaderRoundTrips() {
    ForyCodec writer = registerAll(new ForyCodec());
    ForyCodec reader = registerAll(new ForyCodec());

    CallHeader header = new CallHeader("echo", 42, (short) 0, List.of(), List.of());
    byte[] encoded = writer.encode(header);
    CallHeader decoded = (CallHeader) reader.decode(encoded, CallHeader.class);
    assertEquals("echo", decoded.method());
    assertEquals(42, decoded.callId());
  }
}
