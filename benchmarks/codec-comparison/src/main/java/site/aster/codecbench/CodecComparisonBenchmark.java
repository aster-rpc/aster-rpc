package site.aster.codecbench;

import java.util.concurrent.TimeUnit;
import org.apache.fory.Fory;
import org.apache.fory.ThreadSafeFory;
import org.apache.fory.config.Language;
import org.openjdk.jmh.annotations.Benchmark;
import org.openjdk.jmh.annotations.BenchmarkMode;
import org.openjdk.jmh.annotations.Fork;
import org.openjdk.jmh.annotations.Measurement;
import org.openjdk.jmh.annotations.Mode;
import org.openjdk.jmh.annotations.OutputTimeUnit;
import org.openjdk.jmh.annotations.Scope;
import org.openjdk.jmh.annotations.Setup;
import org.openjdk.jmh.annotations.State;
import org.openjdk.jmh.annotations.Warmup;
import org.openjdk.jmh.infra.Blackhole;
import site.aster.codecbench.proto.MissionControlProto;
import site.aster.codecbench.types.LogEntry;
import site.aster.codecbench.types.StatusRequest;
import site.aster.codecbench.types.StatusResponse;
import site.aster.codecbench.types.SubmitLogResult;

/**
 * JMH benchmark: Apache Fory (xlang, ref-tracking) vs Protobuf for the Mission Control payloads.
 *
 * <p>Fory is configured identically to {@code site.aster.codec.ForyCodec}: {@code Language.XLANG}
 * and {@code refTracking=true}, backed by {@code ThreadPoolFory}. This matches what the Aster
 * Java binding uses on the wire, so the numbers here are directly comparable to the codec cost
 * inside an Aster RPC.
 *
 * <p>Run: {@code mvn -f benchmarks/codec-comparison/pom.xml package} then
 * {@code java -jar benchmarks/codec-comparison/target/benchmarks.jar}.
 */
@State(Scope.Benchmark)
@BenchmarkMode(Mode.AverageTime)
@OutputTimeUnit(TimeUnit.NANOSECONDS)
@Warmup(iterations = 3, time = 1, timeUnit = TimeUnit.SECONDS)
@Measurement(iterations = 5, time = 1, timeUnit = TimeUnit.SECONDS)
@Fork(1)
public class CodecComparisonBenchmark {

  private ThreadSafeFory fory;

  // Fory payloads (records)
  private StatusRequest foryStatusReq;
  private StatusResponse foryStatusResp;
  private LogEntry foryLogEntry;
  private SubmitLogResult forySubmitResult;

  // Pre-encoded Fory byte[]s (for decode benchmarks)
  private byte[] foryStatusReqBytes;
  private byte[] foryStatusRespBytes;
  private byte[] foryLogEntryBytes;
  private byte[] forySubmitResultBytes;

  // Protobuf payloads
  private MissionControlProto.StatusRequest protoStatusReq;
  private MissionControlProto.StatusResponse protoStatusResp;
  private MissionControlProto.LogEntry protoLogEntry;
  private MissionControlProto.SubmitLogResult protoSubmitResult;

  // Pre-encoded Protobuf byte[]s (for decode benchmarks)
  private byte[] protoStatusReqBytes;
  private byte[] protoStatusRespBytes;
  private byte[] protoLogEntryBytes;
  private byte[] protoSubmitResultBytes;

  @Setup
  public void setup() {
    // Fory setup — mirrors ForyCodec exactly.
    fory =
        Fory.builder().withLanguage(Language.XLANG).withRefTracking(true).buildThreadSafeFory();
    fory.register(StatusRequest.class, StatusRequest.FORY_TAG);
    fory.register(StatusResponse.class, StatusResponse.FORY_TAG);
    fory.register(LogEntry.class, LogEntry.FORY_TAG);
    fory.register(SubmitLogResult.class, SubmitLogResult.FORY_TAG);

    // Shared payloads used by both codecs.
    String agentId = "agent-alpha-01";
    String status = "healthy";
    long uptime = 3600L * 24L * 3L;
    double timestamp = 1_700_000_000.123;
    String level = "INFO";
    String message = "system nominal; all checks passed within expected thresholds";
    boolean accepted = true;

    foryStatusReq = new StatusRequest(agentId);
    foryStatusResp = new StatusResponse(agentId, status, uptime);
    foryLogEntry = new LogEntry(timestamp, level, message, agentId);
    forySubmitResult = new SubmitLogResult(accepted);

    foryStatusReqBytes = fory.serialize(foryStatusReq);
    foryStatusRespBytes = fory.serialize(foryStatusResp);
    foryLogEntryBytes = fory.serialize(foryLogEntry);
    forySubmitResultBytes = fory.serialize(forySubmitResult);

    protoStatusReq = MissionControlProto.StatusRequest.newBuilder().setAgentId(agentId).build();
    protoStatusResp =
        MissionControlProto.StatusResponse.newBuilder()
            .setAgentId(agentId)
            .setStatus(status)
            .setUptimeSecs(uptime)
            .build();
    protoLogEntry =
        MissionControlProto.LogEntry.newBuilder()
            .setTimestamp(timestamp)
            .setLevel(level)
            .setMessage(message)
            .setAgentId(agentId)
            .build();
    protoSubmitResult =
        MissionControlProto.SubmitLogResult.newBuilder().setAccepted(accepted).build();

    protoStatusReqBytes = protoStatusReq.toByteArray();
    protoStatusRespBytes = protoStatusResp.toByteArray();
    protoLogEntryBytes = protoLogEntry.toByteArray();
    protoSubmitResultBytes = protoSubmitResult.toByteArray();

    System.out.println(
        "Wire sizes (bytes):  "
            + String.format(
                "StatusRequest fory=%d proto=%d | StatusResponse fory=%d proto=%d | "
                    + "LogEntry fory=%d proto=%d | SubmitLogResult fory=%d proto=%d",
                foryStatusReqBytes.length,
                protoStatusReqBytes.length,
                foryStatusRespBytes.length,
                protoStatusRespBytes.length,
                foryLogEntryBytes.length,
                protoLogEntryBytes.length,
                forySubmitResultBytes.length,
                protoSubmitResultBytes.length));
  }

  // ─── Encode ─────────────────────────────────────────────────────────────

  @Benchmark
  public byte[] foryEncodeStatusRequest() {
    return fory.serialize(foryStatusReq);
  }

  @Benchmark
  public byte[] protoEncodeStatusRequest() {
    return protoStatusReq.toByteArray();
  }

  @Benchmark
  public byte[] foryEncodeStatusResponse() {
    return fory.serialize(foryStatusResp);
  }

  @Benchmark
  public byte[] protoEncodeStatusResponse() {
    return protoStatusResp.toByteArray();
  }

  @Benchmark
  public byte[] foryEncodeLogEntry() {
    return fory.serialize(foryLogEntry);
  }

  @Benchmark
  public byte[] protoEncodeLogEntry() {
    return protoLogEntry.toByteArray();
  }

  @Benchmark
  public byte[] foryEncodeSubmitLogResult() {
    return fory.serialize(forySubmitResult);
  }

  @Benchmark
  public byte[] protoEncodeSubmitLogResult() {
    return protoSubmitResult.toByteArray();
  }

  // ─── Decode ─────────────────────────────────────────────────────────────

  @Benchmark
  public Object foryDecodeStatusRequest() {
    return fory.deserialize(foryStatusReqBytes);
  }

  @Benchmark
  public MissionControlProto.StatusRequest protoDecodeStatusRequest() throws Exception {
    return MissionControlProto.StatusRequest.parseFrom(protoStatusReqBytes);
  }

  @Benchmark
  public Object foryDecodeStatusResponse() {
    return fory.deserialize(foryStatusRespBytes);
  }

  @Benchmark
  public MissionControlProto.StatusResponse protoDecodeStatusResponse() throws Exception {
    return MissionControlProto.StatusResponse.parseFrom(protoStatusRespBytes);
  }

  @Benchmark
  public Object foryDecodeLogEntry() {
    return fory.deserialize(foryLogEntryBytes);
  }

  @Benchmark
  public MissionControlProto.LogEntry protoDecodeLogEntry() throws Exception {
    return MissionControlProto.LogEntry.parseFrom(protoLogEntryBytes);
  }

  @Benchmark
  public Object foryDecodeSubmitLogResult() {
    return fory.deserialize(forySubmitResultBytes);
  }

  @Benchmark
  public MissionControlProto.SubmitLogResult protoDecodeSubmitLogResult() throws Exception {
    return MissionControlProto.SubmitLogResult.parseFrom(protoSubmitResultBytes);
  }

  // ─── Round-trip (encode + decode) ───────────────────────────────────────
  // These approximate what one RPC costs on the codec side (client encodes, server decodes).

  @Benchmark
  public void foryRoundTripStatusResponse(Blackhole bh) {
    byte[] encoded = fory.serialize(foryStatusResp);
    bh.consume(fory.deserialize(encoded));
  }

  @Benchmark
  public void protoRoundTripStatusResponse(Blackhole bh) throws Exception {
    byte[] encoded = protoStatusResp.toByteArray();
    bh.consume(MissionControlProto.StatusResponse.parseFrom(encoded));
  }

  @Benchmark
  public void foryRoundTripLogEntry(Blackhole bh) {
    byte[] encoded = fory.serialize(foryLogEntry);
    bh.consume(fory.deserialize(encoded));
  }

  @Benchmark
  public void protoRoundTripLogEntry(Blackhole bh) throws Exception {
    byte[] encoded = protoLogEntry.toByteArray();
    bh.consume(MissionControlProto.LogEntry.parseFrom(encoded));
  }
}
