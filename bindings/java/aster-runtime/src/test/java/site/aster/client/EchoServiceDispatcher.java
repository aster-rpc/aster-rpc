package site.aster.client;

import java.util.Map;
import org.apache.fory.Fory;
import site.aster.annotations.Scope;
import site.aster.codec.Codec;
import site.aster.interceptors.CallContext;
import site.aster.server.spi.MethodDescriptor;
import site.aster.server.spi.MethodDispatcher;
import site.aster.server.spi.RequestStyle;
import site.aster.server.spi.ResponseStream;
import site.aster.server.spi.ServerStreamDispatcher;
import site.aster.server.spi.ServiceDescriptor;
import site.aster.server.spi.ServiceDispatcher;
import site.aster.server.spi.StreamingKind;
import site.aster.server.spi.UnaryDispatcher;

/**
 * Hand-written {@link ServiceDispatcher} used by {@link AsterClientUnaryE2ETest}. Stands in for the
 * output of {@code aster-codegen-apt} so the client round-trip can be exercised without pulling the
 * whole annotation-processor pipeline into test scope.
 *
 * <p>Registered via {@code META-INF/services/site.aster.server.spi.ServiceDispatcher} under test
 * resources.
 */
public final class EchoServiceDispatcher implements ServiceDispatcher {

  public static final String SERVICE_NAME = "EchoService";
  public static final String METHOD_NAME = "echo";
  public static final String STREAM_METHOD_NAME = "echoStream";
  public static final String REQ_TYPE_TAG = "_aster_test/EchoRequest";
  public static final String RESP_TYPE_TAG = "_aster_test/EchoResponse";
  public static final String STREAM_REQ_TYPE_TAG = "_aster_test/EchoStreamRequest";

  public record EchoRequest(String message) {}

  public record EchoResponse(String message) {}

  /** Server-streaming request: produce {@code count} echoes of {@code message}. */
  public record EchoStreamRequest(String message, int count) {}

  public static final class Impl {
    public EchoResponse echo(EchoRequest request) {
      return new EchoResponse("echo:" + request.message());
    }

    public void echoStream(EchoStreamRequest request, ResponseStream out, Codec codec)
        throws Exception {
      for (int i = 0; i < request.count(); i++) {
        EchoResponse resp = new EchoResponse("stream:" + request.message() + ":" + i);
        out.send(codec.encode(resp));
      }
    }
  }

  private final ServiceDescriptor descriptor =
      new ServiceDescriptor(SERVICE_NAME, 1, Scope.SHARED, Impl.class);

  private final MethodDescriptor methodDescriptor =
      new MethodDescriptor(
          METHOD_NAME,
          StreamingKind.UNARY,
          RequestStyle.EXPLICIT,
          REQ_TYPE_TAG,
          java.util.List.of(),
          RESP_TYPE_TAG,
          false,
          false);

  private final MethodDescriptor streamMethodDescriptor =
      new MethodDescriptor(
          STREAM_METHOD_NAME,
          StreamingKind.SERVER_STREAM,
          RequestStyle.EXPLICIT,
          STREAM_REQ_TYPE_TAG,
          java.util.List.of(),
          RESP_TYPE_TAG,
          false,
          false);

  private final UnaryDispatcher unary =
      new UnaryDispatcher() {
        @Override
        public MethodDescriptor descriptor() {
          return methodDescriptor;
        }

        @Override
        public byte[] invoke(Object impl, byte[] requestBytes, Codec codec, CallContext ctx) {
          EchoRequest req = (EchoRequest) codec.decode(requestBytes, EchoRequest.class);
          EchoResponse resp = ((Impl) impl).echo(req);
          return codec.encode(resp);
        }
      };

  private final ServerStreamDispatcher serverStream =
      new ServerStreamDispatcher() {
        @Override
        public MethodDescriptor descriptor() {
          return streamMethodDescriptor;
        }

        @Override
        public void invoke(
            Object impl, byte[] requestBytes, Codec codec, CallContext ctx, ResponseStream out)
            throws Exception {
          EchoStreamRequest req =
              (EchoStreamRequest) codec.decode(requestBytes, EchoStreamRequest.class);
          ((Impl) impl).echoStream(req, out, codec);
        }
      };

  @Override
  public ServiceDescriptor descriptor() {
    return descriptor;
  }

  @Override
  public Map<String, MethodDispatcher> methods() {
    return Map.of(METHOD_NAME, unary, STREAM_METHOD_NAME, serverStream);
  }

  @Override
  public void registerTypes(Fory fory) {
    try {
      fory.register(EchoRequest.class, REQ_TYPE_TAG);
    } catch (Throwable ignored) {
    }
    try {
      fory.register(EchoResponse.class, RESP_TYPE_TAG);
    } catch (Throwable ignored) {
    }
    try {
      fory.register(EchoStreamRequest.class, STREAM_REQ_TYPE_TAG);
    } catch (Throwable ignored) {
    }
  }
}
