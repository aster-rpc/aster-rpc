package site.aster.examples.missioncontrol;

import java.util.LinkedHashMap;
import java.util.List;
import java.util.Map;
import org.apache.fory.Fory;
import site.aster.annotations.Scope;
import site.aster.codec.Codec;
import site.aster.examples.missioncontrol.types.Assignment;
import site.aster.examples.missioncontrol.types.Heartbeat;
import site.aster.interceptors.CallContext;
import site.aster.server.spi.MethodDescriptor;
import site.aster.server.spi.MethodDispatcher;
import site.aster.server.spi.RequestStyle;
import site.aster.server.spi.ServiceDescriptor;
import site.aster.server.spi.ServiceDispatcher;
import site.aster.server.spi.StreamingKind;
import site.aster.server.spi.UnaryDispatcher;

/**
 * Hand-written {@link ServiceDispatcher} for {@link AgentSession}. SESSION-scoped — the runtime
 * allocates one instance per (peerId, streamId) pair via the user-supplied factory.
 *
 * <p>Implements {@code register} and {@code heartbeat}; the bidi-streaming {@code runCommand}
 * method from the Python sample is omitted until reactor read-side multi-frame support lands.
 */
public final class AgentSessionDispatcher implements ServiceDispatcher {

  public static final String SERVICE_NAME = "AgentSession";
  public static final int SERVICE_VERSION = 1;

  private final ServiceDescriptor descriptor =
      new ServiceDescriptor(SERVICE_NAME, SERVICE_VERSION, Scope.SESSION, AgentSession.class);

  private final Map<String, MethodDispatcher> methods;

  public AgentSessionDispatcher() {
    LinkedHashMap<String, MethodDispatcher> m = new LinkedHashMap<>();
    m.put("register", new Register());
    m.put("heartbeat", new HeartbeatRpc());
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
    safeRegister(fory, Heartbeat.class, Heartbeat.FORY_TAG);
    safeRegister(fory, Assignment.class, Assignment.FORY_TAG);
  }

  private static void safeRegister(Fory fory, Class<?> cls, String tag) {
    try {
      fory.register(cls, tag);
    } catch (Throwable ignored) {
      // duplicate registration is fine
    }
  }

  // ─── Method dispatchers ────────────────────────────────────────────────────

  private static final class Register implements UnaryDispatcher {
    private static final MethodDescriptor DESCRIPTOR =
        new MethodDescriptor(
            "register",
            StreamingKind.UNARY,
            RequestStyle.EXPLICIT,
            Heartbeat.FORY_TAG,
            List.of(),
            Assignment.FORY_TAG,
            false,
            false);

    @Override
    public MethodDescriptor descriptor() {
      return DESCRIPTOR;
    }

    @Override
    public byte[] invoke(Object impl, byte[] requestBytes, Codec codec, CallContext ctx) {
      Heartbeat hb = (Heartbeat) codec.decode(requestBytes, Heartbeat.class);
      Assignment assignment = ((AgentSession) impl).register(hb);
      return codec.encode(assignment);
    }
  }

  private static final class HeartbeatRpc implements UnaryDispatcher {
    private static final MethodDescriptor DESCRIPTOR =
        new MethodDescriptor(
            "heartbeat",
            StreamingKind.UNARY,
            RequestStyle.EXPLICIT,
            Heartbeat.FORY_TAG,
            List.of(),
            Assignment.FORY_TAG,
            false,
            true);

    @Override
    public MethodDescriptor descriptor() {
      return DESCRIPTOR;
    }

    @Override
    public byte[] invoke(Object impl, byte[] requestBytes, Codec codec, CallContext ctx) {
      Heartbeat hb = (Heartbeat) codec.decode(requestBytes, Heartbeat.class);
      Assignment assignment = ((AgentSession) impl).heartbeat(hb);
      return codec.encode(assignment);
    }
  }
}
