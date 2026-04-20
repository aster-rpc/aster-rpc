package site.aster.examples.missioncontrol;

import java.util.ArrayList;
import java.util.List;
import java.util.concurrent.LinkedBlockingQueue;
import java.util.concurrent.TimeUnit;
import site.aster.codec.Codec;
import site.aster.examples.missioncontrol.types.IngestResult;
import site.aster.examples.missioncontrol.types.LogEntry;
import site.aster.examples.missioncontrol.types.MetricPoint;
import site.aster.examples.missioncontrol.types.StatusRequest;
import site.aster.examples.missioncontrol.types.StatusResponse;
import site.aster.examples.missioncontrol.types.SubmitLogResult;
import site.aster.examples.missioncontrol.types.TailRequest;
import site.aster.server.spi.RequestStream;
import site.aster.server.spi.ResponseStream;

/**
 * Fleet-wide mission-control service. SHARED scope — one instance, all peers see the same state.
 * Mirrors the Python {@code MissionControl} class in {@code examples/python/mission_control}.
 *
 * <p>Implements all four Python methods:
 *
 * <ul>
 *   <li>{@code getStatus} — unary
 *   <li>{@code submitLog} — unary
 *   <li>{@code tailLogs} — server-streaming
 *   <li>{@code ingestMetrics} — client-streaming
 * </ul>
 */
public final class MissionControl {

  private static final java.util.Map<String, Integer> LOG_LEVEL_RANK =
      java.util.Map.of("debug", 0, "info", 1, "warn", 2, "error", 3);

  private final LinkedBlockingQueue<LogEntry> logQueue = new LinkedBlockingQueue<>();
  private final List<MetricPoint> metrics = new ArrayList<>();
  private final long startNanos = System.nanoTime();

  public StatusResponse getStatus(StatusRequest req) {
    long uptimeSecs = (System.nanoTime() - startNanos) / 1_000_000_000L;
    return new StatusResponse(req.agentId(), "running", uptimeSecs);
  }

  public SubmitLogResult submitLog(LogEntry entry) {
    logQueue.offer(entry);
    return new SubmitLogResult(true);
  }

  /**
   * Drain matching log entries from the queue forever, matching the Python equivalent's {@code
   * while True: yield await queue.get()} semantics. Cross-binding tests open the stream, submit a
   * log entry AFTER the stream is open, and then close / cancel to stop consuming — an idle timeout
   * here races the open-then-submit pattern (clients spend ~300ms opening the stream, so a 250ms
   * idle-exit returns the stream before the first entry arrives).
   *
   * <p>The short poll interval keeps {@link ResponseStream#isCancelled()} observable so that when a
   * consumer closes its stream (Kotlin {@code ServerStreamCall.close}, Python / TS {@code break}
   * out of the async-for) the handler exits on the next tick instead of blocking on {@code take()}
   * forever.
   */
  public void tailLogs(TailRequest req, ResponseStream out, Codec codec) throws Exception {
    // JSON-mode clients can omit fields that match the dataclass default in their binding
    // (Python sends {"level":"info"} with no agent_id when the user didn't set one). Jackson
    // leaves those String fields null on a Java record, so null-coalesce them before use.
    String reqAgentId = req.agentId() == null ? "" : req.agentId();
    String reqLevel = req.level() == null ? "info" : req.level();
    int minRank = LOG_LEVEL_RANK.getOrDefault(reqLevel.toLowerCase(java.util.Locale.ROOT), 0);
    while (!out.isCancelled()) {
      LogEntry entry = logQueue.poll(100L, TimeUnit.MILLISECONDS);
      if (entry == null) {
        continue;
      }
      String entryAgentId = entry.agentId() == null ? "" : entry.agentId();
      if (!reqAgentId.isEmpty() && !reqAgentId.equals(entryAgentId)) {
        continue;
      }
      String entryLevel = entry.level() == null ? "info" : entry.level();
      int entryRank = LOG_LEVEL_RANK.getOrDefault(entryLevel.toLowerCase(java.util.Locale.ROOT), 0);
      if (entryRank < minRank) {
        continue;
      }
      out.send(codec.encode(entry));
    }
  }

  /**
   * Drain a stream of {@link MetricPoint}s from the agent and accumulate them. Mirrors the Python
   * {@code ingestMetrics} client-streaming method. Returns once {@code in.receive()} reports
   * end-of-stream.
   */
  public IngestResult ingestMetrics(RequestStream in, Codec codec) throws Exception {
    int accepted = 0;
    while (true) {
      byte[] payload = in.receive();
      if (payload == null) {
        return new IngestResult(accepted, 0);
      }
      MetricPoint point = (MetricPoint) codec.decode(payload, MetricPoint.class);
      synchronized (metrics) {
        metrics.add(point);
      }
      accepted++;
    }
  }

  public LinkedBlockingQueue<LogEntry> logQueue() {
    return logQueue;
  }

  public List<MetricPoint> metricsSnapshot() {
    synchronized (metrics) {
      return List.copyOf(metrics);
    }
  }
}
