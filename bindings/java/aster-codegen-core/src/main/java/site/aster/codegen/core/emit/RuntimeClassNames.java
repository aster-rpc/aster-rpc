package site.aster.codegen.core.emit;

import com.palantir.javapoet.ClassName;

/** Cached {@link ClassName} references into {@code aster-runtime}. */
public final class RuntimeClassNames {

  public static final String SPI_PKG = "site.aster.server.spi";
  public static final String SESSION_PKG = "site.aster.server.session";
  public static final String CODEC_PKG = "site.aster.codec";
  public static final String INTERCEPTORS_PKG = "site.aster.interceptors";
  public static final String ANNOTATIONS_PKG = "site.aster.annotations";
  public static final String FORY_PKG = "org.apache.fory";

  public static final ClassName SERVICE_DISPATCHER = ClassName.get(SPI_PKG, "ServiceDispatcher");
  public static final ClassName METHOD_DISPATCHER = ClassName.get(SPI_PKG, "MethodDispatcher");
  public static final ClassName UNARY_DISPATCHER = ClassName.get(SPI_PKG, "UnaryDispatcher");
  public static final ClassName SERVER_STREAM_DISPATCHER =
      ClassName.get(SPI_PKG, "ServerStreamDispatcher");
  public static final ClassName CLIENT_STREAM_DISPATCHER =
      ClassName.get(SPI_PKG, "ClientStreamDispatcher");
  public static final ClassName BIDI_STREAM_DISPATCHER =
      ClassName.get(SPI_PKG, "BidiStreamDispatcher");
  public static final ClassName METHOD_DESCRIPTOR = ClassName.get(SPI_PKG, "MethodDescriptor");
  public static final ClassName PARAM_DESCRIPTOR = ClassName.get(SPI_PKG, "ParamDescriptor");
  public static final ClassName SERVICE_DESCRIPTOR = ClassName.get(SPI_PKG, "ServiceDescriptor");
  public static final ClassName REQUEST_STYLE = ClassName.get(SPI_PKG, "RequestStyle");
  public static final ClassName STREAMING_KIND = ClassName.get(SPI_PKG, "StreamingKind");
  public static final ClassName RESPONSE_STREAM = ClassName.get(SPI_PKG, "ResponseStream");
  public static final ClassName REQUEST_STREAM = ClassName.get(SPI_PKG, "RequestStream");

  public static final ClassName CODEC = ClassName.get(CODEC_PKG, "Codec");
  public static final ClassName CALL_CONTEXT = ClassName.get(INTERCEPTORS_PKG, "CallContext");
  public static final ClassName SCOPE = ClassName.get(ANNOTATIONS_PKG, "Scope");
  public static final ClassName FORY = ClassName.get(FORY_PKG, "Fory");

  public static final ClassName METHOD_METADATA = ClassName.get(SPI_PKG, "MethodMetadata");
  public static final ClassName FIELD_METADATA = ClassName.get(SPI_PKG, "FieldMetadata");

  private RuntimeClassNames() {}
}
