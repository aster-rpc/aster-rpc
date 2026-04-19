package site.aster.examples.missioncontrol.auth;

import java.util.LinkedHashMap;
import java.util.List;
import java.util.Map;
import org.apache.fory.Fory;
import site.aster.annotations.Scope;
import site.aster.codec.Codec;
import site.aster.contract.Capabilities;
import site.aster.examples.missioncontrol.AgentSession;
import site.aster.examples.missioncontrol.Role;
import site.aster.examples.missioncontrol.types.Assignment;
import site.aster.examples.missioncontrol.types.Command;
import site.aster.examples.missioncontrol.types.CommandResult;
import site.aster.examples.missioncontrol.types.Heartbeat;
import site.aster.interceptors.CallContext;
import site.aster.server.spi.BidiStreamDispatcher;
import site.aster.server.spi.MethodDescriptor;
import site.aster.server.spi.MethodDispatcher;
import site.aster.server.spi.RequestStream;
import site.aster.server.spi.RequestStyle;
import site.aster.server.spi.ResponseStream;
import site.aster.server.spi.ServiceDescriptor;
import site.aster.server.spi.ServiceDispatcher;
import site.aster.server.spi.StreamingKind;
import site.aster.server.spi.UnaryDispatcher;

/**
 * Auth-mode {@link ServiceDispatcher} for {@link AgentSession}. Mirrors the role map from {@code
 * examples/python/mission_control/services_auth.py}:
 *
 * <ul>
 *   <li>{@code register} — {@link Role#INGEST}
 *   <li>{@code heartbeat} — public
 *   <li>{@code runCommand} — {@link Role#ADMIN}
 *   <li>{@code chaosFail} — public (test-only; kept ungated to preserve chaos-suite semantics)
 * </ul>
 */
public final class AgentSessionAuthDispatcher implements ServiceDispatcher {

  public static final String SERVICE_NAME = "AgentSession";
  public static final int SERVICE_VERSION = 1;

  private final ServiceDescriptor descriptor =
      new ServiceDescriptor(SERVICE_NAME, SERVICE_VERSION, Scope.SESSION, AgentSession.class, null);

  private final Map<String, MethodDispatcher> methods;

  public AgentSessionAuthDispatcher() {
    LinkedHashMap<String, MethodDispatcher> m = new LinkedHashMap<>();
    m.put("register", new Register());
    m.put("heartbeat", new HeartbeatRpc());
    m.put("runCommand", new RunCommand());
    m.put("chaosFail", new ChaosFail());
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
    m.put("register", Heartbeat.class);
    m.put("heartbeat", Heartbeat.class);
    m.put("runCommand", Command.class);
    m.put("chaosFail", Heartbeat.class);
    return Map.copyOf(m);
  }

  @Override
  public Map<String, Class<?>> responseClasses() {
    LinkedHashMap<String, Class<?>> m = new LinkedHashMap<>();
    m.put("register", Assignment.class);
    m.put("heartbeat", Assignment.class);
    m.put("runCommand", CommandResult.class);
    m.put("chaosFail", Assignment.class);
    return Map.copyOf(m);
  }

  @Override
  public void registerTypes(Fory fory) {
    safeRegister(fory, Heartbeat.class, Heartbeat.FORY_TAG);
    safeRegister(fory, Assignment.class, Assignment.FORY_TAG);
    safeRegister(fory, Command.class, Command.FORY_TAG);
    safeRegister(fory, CommandResult.class, CommandResult.FORY_TAG);
  }

  private static void safeRegister(Fory fory, Class<?> cls, String tag) {
    try {
      site.aster.codec.ForyTags.register(fory, cls, tag);
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
            false,
            Capabilities.role(Role.INGEST));

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
            true,
            null);

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

  private static final class RunCommand implements BidiStreamDispatcher {
    private static final MethodDescriptor DESCRIPTOR =
        new MethodDescriptor(
            "runCommand",
            StreamingKind.BIDI_STREAM,
            RequestStyle.EXPLICIT,
            Command.FORY_TAG,
            List.of(),
            CommandResult.FORY_TAG,
            false,
            false,
            Capabilities.role(Role.ADMIN));

    @Override
    public MethodDescriptor descriptor() {
      return DESCRIPTOR;
    }

    @Override
    public void invoke(
        Object impl, RequestStream in, Codec codec, CallContext ctx, ResponseStream out)
        throws Exception {
      ((AgentSession) impl).runCommand(in, out, codec);
    }
  }

  private static final class ChaosFail implements UnaryDispatcher {
    private static final MethodDescriptor DESCRIPTOR =
        new MethodDescriptor(
            "chaosFail",
            StreamingKind.UNARY,
            RequestStyle.EXPLICIT,
            Heartbeat.FORY_TAG,
            List.of(),
            Assignment.FORY_TAG,
            false,
            false,
            null);

    @Override
    public MethodDescriptor descriptor() {
      return DESCRIPTOR;
    }

    @Override
    public byte[] invoke(Object impl, byte[] requestBytes, Codec codec, CallContext ctx) {
      Heartbeat hb = (Heartbeat) codec.decode(requestBytes, Heartbeat.class);
      Assignment assignment = ((AgentSession) impl).chaosFail(hb);
      return codec.encode(assignment);
    }
  }
}
