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
 * <p>Implements three of the five Python methods:
 *
 * <ul>
 *   <li>{@code getStatus} — unary
 *   <li>{@code submitLog} — unary
 *   <li>{@code tailLogs} — server-streaming
 * </ul>
 *
 * <p>The {@code ingestMetrics} client-streaming method is omitted until reactor read-side
 * multi-frame support lands.
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
   * Drain matching log entries from the queue and write them to {@code out} as encoded frames. The
   * Python equivalent yields entries forever; this Java port runs until the queue has been quiet
   * for {@code idleTimeoutMs} milliseconds, which is the natural fit for the current
   * blocking-virtual-thread server-stream invocation model. A reactive Flow.Publisher version is
   * future work.
   */
  public void tailLogs(TailRequest req, ResponseStream out, Codec codec) throws Exception {
    int minRank = LOG_LEVEL_RANK.getOrDefault(req.level().toLowerCase(java.util.Locale.ROOT), 0);
    long idleTimeoutMs = 250L;
    while (!out.isCancelled()) {
      LogEntry entry = logQueue.poll(idleTimeoutMs, TimeUnit.MILLISECONDS);
      if (entry == null) {
        return;
      }
      if (!req.agentId().isEmpty() && !req.agentId().equals(entry.agentId())) {
        continue;
      }
      int entryRank =
          LOG_LEVEL_RANK.getOrDefault(entry.level().toLowerCase(java.util.Locale.ROOT), 0);
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
