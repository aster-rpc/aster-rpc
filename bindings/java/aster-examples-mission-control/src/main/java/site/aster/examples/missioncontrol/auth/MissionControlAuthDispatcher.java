package site.aster.examples.missioncontrol.auth;

import java.util.LinkedHashMap;
import java.util.List;
import java.util.Map;
import org.apache.fory.Fory;
import site.aster.annotations.Scope;
import site.aster.codec.Codec;
import site.aster.contract.Capabilities;
import site.aster.contract.CapabilityRequirement;
import site.aster.examples.missioncontrol.MissionControl;
import site.aster.examples.missioncontrol.Role;
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
 * Auth-mode {@link ServiceDispatcher} for {@link MissionControl}. Wire-identical to {@code
 * MissionControlDispatcher} but annotates each {@link MethodDescriptor} with a {@link
 * CapabilityRequirement} so {@code CapabilityInterceptor} can gate the call. Mirrors the role
 * assignment in {@code examples/python/mission_control/services_auth.py}:
 *
 * <ul>
 *   <li>{@code getStatus} — {@link Role#STATUS}
 *   <li>{@code submitLog} — public (no requires)
 *   <li>{@code tailLogs} — any-of({@link Role#LOGS}, {@link Role#ADMIN})
 *   <li>{@code ingestMetrics} — {@link Role#INGEST}
 * </ul>
 *
 * <p>Not registered via {@code META-INF/services}; the {@code ServerAuth} bootstrap wires it
 * explicitly via the {@link site.aster.server.AsterServer.Builder#service(Object,
 * ServiceDispatcher)} overload.
 */
public final class MissionControlAuthDispatcher implements ServiceDispatcher {

  public static final String SERVICE_NAME = "MissionControl";
  public static final int SERVICE_VERSION = 1;

  private final ServiceDescriptor descriptor =
      new ServiceDescriptor(
          SERVICE_NAME, SERVICE_VERSION, Scope.SHARED, MissionControl.class, null);

  private final Map<String, MethodDispatcher> methods;

  public MissionControlAuthDispatcher() {
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
  public Map<String, Class<?>> requestClasses() {
    LinkedHashMap<String, Class<?>> m = new LinkedHashMap<>();
    m.put("getStatus", StatusRequest.class);
    m.put("submitLog", LogEntry.class);
    m.put("tailLogs", TailRequest.class);
    m.put("ingestMetrics", MetricPoint.class);
    return Map.copyOf(m);
  }

  @Override
  public Map<String, Class<?>> responseClasses() {
    LinkedHashMap<String, Class<?>> m = new LinkedHashMap<>();
    m.put("getStatus", StatusResponse.class);
    m.put("submitLog", SubmitLogResult.class);
    m.put("ingestMetrics", IngestResult.class);
    return Map.copyOf(m);
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
      site.aster.codec.ForyTags.register(fory, cls, tag);
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
            true,
            Capabilities.role(Role.STATUS));

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
            false,
            null);

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
            false,
            Capabilities.role(Role.INGEST));

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
            true,
            Capabilities.anyOf(Role.LOGS, Role.ADMIN));

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
