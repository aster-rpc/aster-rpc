package site.aster.examples.missioncontrol;

import java.util.LinkedHashMap;
import java.util.List;
import java.util.Map;
import org.apache.fory.Fory;
import site.aster.annotations.Scope;
import site.aster.codec.Codec;
import site.aster.examples.missioncontrol.types.IngestResult;
import site.aster.examples.missioncontrol.types.LogEntry;
import site.aster.examples.missioncontrol.types.MetricPoint;
import site.aster.examples.missioncontrol.types.StatusRequest;
import site.aster.examples.missioncontrol.types.StatusResponse;
import site.aster.examples.missioncontrol.types.SubmitLogResult;
import site.aster.examples.missioncontrol.types.TailRequest;
import site.aster.interceptors.CallContext;
import site.aster.server.spi.ClientStreamDispatcher;
import site.aster.server.spi.MethodDescriptor;
import site.aster.server.spi.MethodDispatcher;
import site.aster.server.spi.RequestStream;
import site.aster.server.spi.RequestStyle;
import site.aster.server.spi.ResponseStream;
import site.aster.server.spi.ServerStreamDispatcher;
import site.aster.server.spi.ServiceDescriptor;
import site.aster.server.spi.ServiceDispatcher;
import site.aster.server.spi.StreamingKind;
import site.aster.server.spi.UnaryDispatcher;

/**
 * Hand-written {@link ServiceDispatcher} for {@link MissionControl}. Stands in for the not-yet
 * extended {@code DispatcherEmitter} (which currently emits stubs for streaming methods); when
 * codegen-apt grows real server-stream support this class can be replaced with the generated
 * equivalent.
 *
 * <p>Wire identity matches {@code examples/python/mission_control/services.py}: service name {@code
 * MissionControl}, version {@code 1}, methods {@code getStatus} / {@code submitLog} / {@code
 * tailLogs} / {@code ingestMetrics}.
 */
public final class MissionControlDispatcher implements ServiceDispatcher {

  public static final String SERVICE_NAME = "MissionControl";
  public static final int SERVICE_VERSION = 1;

  private final ServiceDescriptor descriptor =
      new ServiceDescriptor(SERVICE_NAME, SERVICE_VERSION, Scope.SHARED, MissionControl.class);

  private final Map<String, MethodDispatcher> methods;

  public MissionControlDispatcher() {
    LinkedHashMap<String, MethodDispatcher> m = new LinkedHashMap<>();
    m.put("getStatus", new GetStatus());
    m.put("submitLog", new SubmitLog());
    m.put("tailLogs", new TailLogs());
    m.put("ingestMetrics", new IngestMetrics());
    this.methods = Map.copyOf(m);
  }

  @Override
  public ServiceDescriptor descriptor() {
    return descriptor;
  }

  @Override
  public Map<String, MethodDispatcher> methods() {
    return methods;
  }

  @Override
  public void registerTypes(Fory fory) {
    safeRegister(fory, StatusRequest.class, StatusRequest.FORY_TAG);
    safeRegister(fory, StatusResponse.class, StatusResponse.FORY_TAG);
    safeRegister(fory, LogEntry.class, LogEntry.FORY_TAG);
    safeRegister(fory, SubmitLogResult.class, SubmitLogResult.FORY_TAG);
    safeRegister(fory, TailRequest.class, TailRequest.FORY_TAG);
    safeRegister(fory, MetricPoint.class, MetricPoint.FORY_TAG);
    safeRegister(fory, IngestResult.class, IngestResult.FORY_TAG);
  }

  private static void safeRegister(Fory fory, Class<?> cls, String tag) {
    try {
      fory.register(cls, tag);
    } catch (Throwable ignored) {
      // duplicate registration is fine
    }
  }

  // ─── Method dispatchers ────────────────────────────────────────────────────

  private static final class GetStatus implements UnaryDispatcher {
    private static final MethodDescriptor DESCRIPTOR =
        new MethodDescriptor(
            "getStatus",
            StreamingKind.UNARY,
            RequestStyle.EXPLICIT,
            StatusRequest.FORY_TAG,
            List.of(),
            StatusResponse.FORY_TAG,
            false,
            true);

    @Override
    public MethodDescriptor descriptor() {
      return DESCRIPTOR;
    }

    @Override
    public byte[] invoke(Object impl, byte[] requestBytes, Codec codec, CallContext ctx) {
      StatusRequest req = (StatusRequest) codec.decode(requestBytes, StatusRequest.class);
      StatusResponse resp = ((MissionControl) impl).getStatus(req);
      return codec.encode(resp);
    }
  }

  private static final class SubmitLog implements UnaryDispatcher {
    private static final MethodDescriptor DESCRIPTOR =
        new MethodDescriptor(
            "submitLog",
            StreamingKind.UNARY,
            RequestStyle.EXPLICIT,
            LogEntry.FORY_TAG,
            List.of(),
            SubmitLogResult.FORY_TAG,
            false,
            false);

    @Override
    public MethodDescriptor descriptor() {
      return DESCRIPTOR;
    }

    @Override
    public byte[] invoke(Object impl, byte[] requestBytes, Codec codec, CallContext ctx) {
      LogEntry entry = (LogEntry) codec.decode(requestBytes, LogEntry.class);
      SubmitLogResult result = ((MissionControl) impl).submitLog(entry);
      return codec.encode(result);
    }
  }

  private static final class IngestMetrics implements ClientStreamDispatcher {
    private static final MethodDescriptor DESCRIPTOR =
        new MethodDescriptor(
            "ingestMetrics",
            StreamingKind.CLIENT_STREAM,
            RequestStyle.EXPLICIT,
            MetricPoint.FORY_TAG,
            List.of(),
            IngestResult.FORY_TAG,
            false,
            false);

    @Override
    public MethodDescriptor descriptor() {
      return DESCRIPTOR;
    }

    @Override
    public byte[] invoke(Object impl, RequestStream in, Codec codec, CallContext ctx)
        throws Exception {
      IngestResult result = ((MissionControl) impl).ingestMetrics(in, codec);
      return codec.encode(result);
    }
  }

  private static final class TailLogs implements ServerStreamDispatcher {
    private static final MethodDescriptor DESCRIPTOR =
        new MethodDescriptor(
            "tailLogs",
            StreamingKind.SERVER_STREAM,
            RequestStyle.EXPLICIT,
            TailRequest.FORY_TAG,
            List.of(),
            LogEntry.FORY_TAG,
            false,
            true);

    @Override
    public MethodDescriptor descriptor() {
      return DESCRIPTOR;
    }

    @Override
    public void invoke(
        Object impl, byte[] requestBytes, Codec codec, CallContext ctx, ResponseStream out)
        throws Exception {
      TailRequest req = (TailRequest) codec.decode(requestBytes, TailRequest.class);
      ((MissionControl) impl).tailLogs(req, out, codec);
    }
  }
}
