package site.aster.codegen.core.emit;

import com.palantir.javapoet.AnnotationSpec;
import com.palantir.javapoet.ArrayTypeName;
import com.palantir.javapoet.ClassName;
import com.palantir.javapoet.CodeBlock;
import com.palantir.javapoet.FieldSpec;
import com.palantir.javapoet.JavaFile;
import com.palantir.javapoet.MethodSpec;
import com.palantir.javapoet.ParameterizedTypeName;
import com.palantir.javapoet.TypeName;
import com.palantir.javapoet.TypeSpec;
import com.palantir.javapoet.WildcardTypeName;
import java.util.Collections;
import java.util.LinkedHashMap;
import java.util.List;
import java.util.Map;
import javax.annotation.processing.Generated;
import javax.lang.model.element.Modifier;
import site.aster.codegen.core.model.FieldModel;
import site.aster.codegen.core.model.MethodModel;
import site.aster.codegen.core.model.ParamModel;
import site.aster.codegen.core.model.RequestStyle;
import site.aster.codegen.core.model.ServiceModel;
import site.aster.codegen.core.model.StreamingKind;

/**
 * Emits the {@code {ServiceSimpleName}$AsterDispatcher} class for a {@link ServiceModel}. One
 * JavaFile per service; inner static classes for each method.
 *
 * <p>The generated class:
 *
 * <ul>
 *   <li>Implements {@code site.aster.server.spi.ServiceDispatcher}
 *   <li>Exposes a static final {@code DESCRIPTOR} for {@code descriptor()}
 *   <li>Builds an immutable {@code LinkedHashMap<String, MethodDispatcher>} in its constructor
 *   <li>Registers every request/response type with Fory in {@code registerTypes(Fory)}
 *   <li>Contains one nested dispatcher class per method, implementing the appropriate sealed
 *       subtype of {@code MethodDispatcher}
 * </ul>
 *
 * <p>Streaming method bodies are emitted as stubs that throw {@link UnsupportedOperationException}
 * — Commit D fills them in with Kotlin Flow / coroutine bridges.
 */
public final class DispatcherEmitter {

  private DispatcherEmitter() {}

  public static JavaFile emit(ServiceModel svc) {
    ClassName dispatcherName = NameConventions.dispatcherClassName(svc);
    ParameterizedTypeName methodsMapType =
        ParameterizedTypeName.get(
            ClassName.get(Map.class),
            ClassName.get(String.class),
            RuntimeClassNames.METHOD_DISPATCHER);

    TypeSpec.Builder type =
        TypeSpec.classBuilder(dispatcherName)
            .addModifiers(Modifier.PUBLIC, Modifier.FINAL)
            .addSuperinterface(RuntimeClassNames.SERVICE_DISPATCHER)
            .addAnnotation(
                AnnotationSpec.builder(Generated.class)
                    .addMember("value", "$S", "site.aster.codegen")
                    .build());

    type.addField(buildDescriptorField(svc));
    type.addField(
        FieldSpec.builder(methodsMapType, "METHODS")
            .addModifiers(Modifier.PRIVATE, Modifier.FINAL)
            .build());

    buildMetadataFields(svc).forEach(type::addField);

    type.addMethod(buildConstructor(svc));
    type.addMethod(
        MethodSpec.methodBuilder("descriptor")
            .addAnnotation(Override.class)
            .addModifiers(Modifier.PUBLIC)
            .returns(RuntimeClassNames.SERVICE_DESCRIPTOR)
            .addStatement("return DESCRIPTOR")
            .build());
    type.addMethod(
        MethodSpec.methodBuilder("methods")
            .addAnnotation(Override.class)
            .addModifiers(Modifier.PUBLIC)
            .returns(methodsMapType)
            .addStatement("return METHODS")
            .build());
    buildMetadataAccessors(svc).forEach(type::addMethod);
    type.addMethod(buildRegisterTypes(svc));
    type.addMethod(buildSafeRegisterHelper());

    for (MethodModel m : svc.methods()) {
      type.addType(buildMethodDispatcher(svc, m));
    }

    return JavaFile.builder(dispatcherName.packageName(), type.build())
        .skipJavaLangImports(true)
        .indent("  ")
        .build();
  }

  private static FieldSpec buildDescriptorField(ServiceModel svc) {
    CodeBlock init =
        CodeBlock.of(
            "new $T($S, $L, $T.$L, $T.class)",
            RuntimeClassNames.SERVICE_DESCRIPTOR,
            svc.name(),
            svc.version(),
            RuntimeClassNames.SCOPE,
            svc.scope().name(),
            svc.implClass());
    return FieldSpec.builder(RuntimeClassNames.SERVICE_DESCRIPTOR, "DESCRIPTOR")
        .addModifiers(Modifier.PRIVATE, Modifier.STATIC, Modifier.FINAL)
        .initializer(init)
        .build();
  }

  private static List<FieldSpec> buildMetadataFields(ServiceModel svc) {
    List<FieldSpec> out = new java.util.ArrayList<>();
    out.add(
        FieldSpec.builder(String.class, "DESCRIPTION")
            .addModifiers(Modifier.PRIVATE, Modifier.STATIC, Modifier.FINAL)
            .initializer("$S", svc.description())
            .build());

    ParameterizedTypeName tagsType =
        ParameterizedTypeName.get(ClassName.get(List.class), ClassName.get(String.class));
    out.add(
        FieldSpec.builder(tagsType, "TAGS")
            .addModifiers(Modifier.PRIVATE, Modifier.STATIC, Modifier.FINAL)
            .initializer(buildStringListLiteral(svc.tags()))
            .build());

    ParameterizedTypeName metaMapType =
        ParameterizedTypeName.get(
            ClassName.get(Map.class),
            ClassName.get(String.class),
            RuntimeClassNames.METHOD_METADATA);
    out.add(
        FieldSpec.builder(metaMapType, "METHOD_METADATA")
            .addModifiers(Modifier.PRIVATE, Modifier.STATIC, Modifier.FINAL)
            .initializer(buildMethodMetadataMap(svc))
            .build());
    return out;
  }

  private static CodeBlock buildMethodMetadataMap(ServiceModel svc) {
    CodeBlock.Builder b = CodeBlock.builder().add("$T.ofEntries(", Map.class);
    boolean first = true;
    for (MethodModel m : svc.methods()) {
      if (isMethodMetadataEmpty(m)) {
        continue;
      }
      if (!first) {
        b.add(",\n      ");
      } else {
        b.add("\n      ");
      }
      first = false;
      b.add(
          "$T.entry($S, new $T($S, $L, $L, $L))",
          Map.class,
          m.wireName(),
          RuntimeClassNames.METHOD_METADATA,
          m.description(),
          buildStringListLiteral(m.tags()),
          m.deprecated(),
          buildFieldMetadataMap(m));
    }
    b.add(")");
    return b.build();
  }

  private static CodeBlock buildFieldMetadataMap(MethodModel m) {
    if (m.fieldMetadata().isEmpty()) {
      return CodeBlock.of("$T.of()", Map.class);
    }
    CodeBlock.Builder b = CodeBlock.builder().add("$T.ofEntries(", Map.class);
    boolean first = true;
    for (FieldModel fm : m.fieldMetadata().values()) {
      if (!first) {
        b.add(",\n        ");
      } else {
        b.add("\n        ");
      }
      first = false;
      b.add(
          "$T.entry($S, new $T($S, $L))",
          Map.class,
          fm.name(),
          RuntimeClassNames.FIELD_METADATA,
          fm.description(),
          buildStringListLiteral(fm.tags()));
    }
    b.add(")");
    return b.build();
  }

  private static boolean isMethodMetadataEmpty(MethodModel m) {
    return m.description().isEmpty()
        && m.tags().isEmpty()
        && !m.deprecated()
        && m.fieldMetadata().isEmpty();
  }

  private static CodeBlock buildStringListLiteral(List<String> items) {
    if (items.isEmpty()) {
      return CodeBlock.of("$T.of()", List.class);
    }
    CodeBlock.Builder b = CodeBlock.builder().add("$T.of(", List.class);
    for (int i = 0; i < items.size(); i++) {
      if (i > 0) {
        b.add(", ");
      }
      b.add("$S", items.get(i));
    }
    b.add(")");
    return b.build();
  }

  private static List<MethodSpec> buildMetadataAccessors(ServiceModel svc) {
    ParameterizedTypeName tagsType =
        ParameterizedTypeName.get(ClassName.get(List.class), ClassName.get(String.class));

    MethodSpec description =
        MethodSpec.methodBuilder("description")
            .addAnnotation(Override.class)
            .addModifiers(Modifier.PUBLIC)
            .returns(String.class)
            .addStatement("return DESCRIPTION")
            .build();
    MethodSpec tags =
        MethodSpec.methodBuilder("tags")
            .addAnnotation(Override.class)
            .addModifiers(Modifier.PUBLIC)
            .returns(tagsType)
            .addStatement("return TAGS")
            .build();
    MethodSpec methodMeta =
        MethodSpec.methodBuilder("methodMetadata")
            .addAnnotation(Override.class)
            .addModifiers(Modifier.PUBLIC)
            .addParameter(String.class, "methodName")
            .returns(RuntimeClassNames.METHOD_METADATA)
            .addStatement(
                "return METHOD_METADATA.getOrDefault(methodName, $T.EMPTY)",
                RuntimeClassNames.METHOD_METADATA)
            .build();
    return List.of(description, tags, methodMeta);
  }

  private static MethodSpec buildConstructor(ServiceModel svc) {
    MethodSpec.Builder ctor = MethodSpec.constructorBuilder().addModifiers(Modifier.PUBLIC);
    ctor.addStatement(
        "$T<$T, $T> m = new $T<>()",
        LinkedHashMap.class,
        String.class,
        RuntimeClassNames.METHOD_DISPATCHER,
        LinkedHashMap.class);
    for (MethodModel method : svc.methods()) {
      String inner = NameConventions.methodDispatcherSimpleName(method);
      ctor.addStatement("m.put($S, new $L())", method.wireName(), inner);
    }
    ctor.addStatement("this.METHODS = $T.unmodifiableMap(m)", Collections.class);
    return ctor.build();
  }

  private static MethodSpec buildRegisterTypes(ServiceModel svc) {
    MethodSpec.Builder b =
        MethodSpec.methodBuilder("registerTypes")
            .addAnnotation(Override.class)
            .addModifiers(Modifier.PUBLIC)
            .addParameter(RuntimeClassNames.FORY, "fory")
            .returns(void.class);

    // Deduplicate request/response/inline-param types across methods. Keyed by class name; same
    // type registered twice is a no-op.
    LinkedHashMap<String, CodeBlock> emits = new LinkedHashMap<>();
    for (MethodModel m : svc.methods()) {
      if (m.requestStyle() == RequestStyle.EXPLICIT && m.requestType() != null) {
        TypeName t = m.requestType();
        String tag = foryTagFor(t, svc);
        emits.putIfAbsent(t.toString(), CodeBlock.of("safeRegister(fory, $T.class, $S)", t, tag));
      } else if (m.requestStyle() == RequestStyle.INLINE) {
        ClassName reqName = NameConventions.inlineRequestClassName(svc, m);
        String tag = NameConventions.inlineRequestForyTag(svc, m);
        emits.putIfAbsent(
            reqName.toString(), CodeBlock.of("safeRegister(fory, $T.class, $S)", reqName, tag));
      }
      if (m.responseType() != null) {
        TypeName t = m.responseType();
        String tag = foryTagFor(t, svc);
        emits.putIfAbsent(t.toString(), CodeBlock.of("safeRegister(fory, $T.class, $S)", t, tag));
      }
      for (ParamModel p : m.inlineParams()) {
        if (p.type() instanceof ClassName cn && !isJavaLangType(cn)) {
          String tag = foryTagFor(cn, svc);
          emits.putIfAbsent(
              cn.toString(), CodeBlock.of("safeRegister(fory, $T.class, $S)", cn, tag));
        }
      }
    }
    for (CodeBlock cb : emits.values()) {
      b.addStatement(cb);
    }
    return b.build();
  }

  private static MethodSpec buildSafeRegisterHelper() {
    // Always passes a tag (empty string = default). The try/catch swallows any registration
    // failure so user pre-registration always wins.
    TypeName classWildcard =
        ParameterizedTypeName.get(
            ClassName.get(Class.class), WildcardTypeName.subtypeOf(Object.class));
    return MethodSpec.methodBuilder("safeRegister")
        .addModifiers(Modifier.PRIVATE, Modifier.STATIC)
        .addParameter(RuntimeClassNames.FORY, "fory")
        .addParameter(classWildcard, "cls")
        .addParameter(String.class, "tag")
        .returns(void.class)
        .beginControlFlow("try")
        .beginControlFlow("if (tag == null || tag.isEmpty())")
        .addStatement("fory.register(cls)")
        .nextControlFlow("else")
        .addStatement("fory.register(cls, tag)")
        .endControlFlow()
        .nextControlFlow("catch ($T ignored)", Throwable.class)
        .endControlFlow()
        .build();
  }

  private static TypeSpec buildMethodDispatcher(ServiceModel svc, MethodModel method) {
    String innerName = NameConventions.methodDispatcherSimpleName(method);
    ClassName parent = dispatcherInterfaceFor(method.streaming());

    TypeSpec.Builder inner =
        TypeSpec.classBuilder(innerName)
            .addModifiers(Modifier.PRIVATE, Modifier.STATIC, Modifier.FINAL)
            .addSuperinterface(parent);

    inner.addField(buildMethodDescriptorField(svc, method));
    inner.addMethod(
        MethodSpec.methodBuilder("descriptor")
            .addAnnotation(Override.class)
            .addModifiers(Modifier.PUBLIC)
            .returns(RuntimeClassNames.METHOD_DESCRIPTOR)
            .addStatement("return DESCRIPTOR")
            .build());

    inner.addMethod(buildInvokeMethod(svc, method));
    return inner.build();
  }

  private static ClassName dispatcherInterfaceFor(StreamingKind kind) {
    return switch (kind) {
      case UNARY -> RuntimeClassNames.UNARY_DISPATCHER;
      case SERVER_STREAM -> RuntimeClassNames.SERVER_STREAM_DISPATCHER;
      case CLIENT_STREAM -> RuntimeClassNames.CLIENT_STREAM_DISPATCHER;
      case BIDI_STREAM -> RuntimeClassNames.BIDI_STREAM_DISPATCHER;
    };
  }

  private static FieldSpec buildMethodDescriptorField(ServiceModel svc, MethodModel method) {
    String requestTag =
        method.requestStyle() == RequestStyle.INLINE
            ? NameConventions.inlineRequestForyTag(svc, method)
            : foryTagFor(method.requestType(), svc);
    String responseTag = foryTagFor(method.responseType(), svc);

    CodeBlock.Builder paramsList = CodeBlock.builder().add("$T.of(", List.class);
    boolean first = true;
    for (ParamModel p : method.inlineParams()) {
      if (!first) {
        paramsList.add(", ");
      }
      first = false;
      paramsList.add(
          "new $T($S, $S, $T.class)",
          RuntimeClassNames.PARAM_DESCRIPTOR,
          p.name(),
          p.type().toString(),
          p.type());
    }
    paramsList.add(")");

    CodeBlock init =
        CodeBlock.of(
            "new $T($S, $T.$L, $T.$L, $S, $L, $S, $L, $L)",
            RuntimeClassNames.METHOD_DESCRIPTOR,
            method.wireName(),
            RuntimeClassNames.STREAMING_KIND,
            method.streaming().name(),
            RuntimeClassNames.REQUEST_STYLE,
            method.requestStyle().name(),
            requestTag,
            paramsList.build(),
            responseTag == null ? "" : responseTag,
            method.hasContextParam(),
            method.idempotent());

    return FieldSpec.builder(RuntimeClassNames.METHOD_DESCRIPTOR, "DESCRIPTOR")
        .addModifiers(Modifier.PRIVATE, Modifier.STATIC, Modifier.FINAL)
        .initializer(init)
        .build();
  }

  private static MethodSpec buildInvokeMethod(ServiceModel svc, MethodModel method) {
    return switch (method.streaming()) {
      case UNARY -> buildUnaryInvoke(svc, method);
      case SERVER_STREAM -> buildServerStreamStub(svc, method);
      case CLIENT_STREAM -> buildClientStreamStub(svc, method);
      case BIDI_STREAM -> buildBidiStreamStub(svc, method);
    };
  }

  private static MethodSpec buildUnaryInvoke(ServiceModel svc, MethodModel method) {
    MethodSpec.Builder b =
        MethodSpec.methodBuilder("invoke")
            .addAnnotation(Override.class)
            .addModifiers(Modifier.PUBLIC)
            .addParameter(Object.class, "impl")
            .addParameter(byte[].class, "requestBytes")
            .addParameter(RuntimeClassNames.CODEC, "codec")
            .addParameter(RuntimeClassNames.CALL_CONTEXT, "ctx")
            .returns(byte[].class)
            .addException(Exception.class);

    // Decode request (if any)
    String decodedName = emitRequestDecode(b, svc, method, "requestBytes");

    // Invoke the user method inside CallContext.runWith so handler code observes the ctx
    CodeBlock callSite = buildCallSite(svc, method, decodedName);
    TypeName responseT =
        method.responseType() != null ? method.responseType() : ClassName.get(Object.class);
    b.addStatement(
        "$T response = $T.runWith(ctx, () -> $L)",
        responseT,
        RuntimeClassNames.CALL_CONTEXT,
        callSite);
    b.addStatement("return codec.encode(response)");
    return b.build();
  }

  private static String emitRequestDecode(
      MethodSpec.Builder b, ServiceModel svc, MethodModel method, String bytesVar) {
    if (method.requestStyle() == RequestStyle.EXPLICIT) {
      TypeName t = method.requestType();
      b.addStatement("$T request = ($T) codec.decode($L, $T.class)", t, t, bytesVar, t);
      return "request";
    }
    // INLINE: synthesized record
    ClassName rec = NameConventions.inlineRequestClassName(svc, method);
    b.addStatement("$T inline = ($T) codec.decode($L, $T.class)", rec, rec, bytesVar, rec);
    return "inline";
  }

  private static CodeBlock buildCallSite(ServiceModel svc, MethodModel method, String decodedName) {
    // svc.method(args...)
    CodeBlock.Builder args = CodeBlock.builder();
    boolean first = true;
    if (method.requestStyle() == RequestStyle.EXPLICIT) {
      args.add(decodedName);
      first = false;
    } else {
      for (ParamModel p : method.inlineParams()) {
        if (!first) {
          args.add(", ");
        }
        first = false;
        args.add("$L.$L()", decodedName, p.name());
      }
    }
    if (method.hasContextParam()) {
      if (!first) {
        args.add(", ");
      }
      args.add("ctx");
    }
    return CodeBlock.of("(($T) impl).$L($L)", svc.implClass(), method.name(), args.build());
  }

  private static MethodSpec buildServerStreamStub(ServiceModel svc, MethodModel method) {
    return streamStubMethod(
        "invoke",
        new StubParam[] {
          new StubParam(Object.class, "impl"),
          new StubParam(byte[].class, "requestBytes"),
          new StubParam(RuntimeClassNames.CODEC, "codec"),
          new StubParam(RuntimeClassNames.CALL_CONTEXT, "ctx"),
          new StubParam(RuntimeClassNames.RESPONSE_STREAM, "out")
        },
        TypeName.VOID);
  }

  private static MethodSpec buildClientStreamStub(ServiceModel svc, MethodModel method) {
    return streamStubMethod(
        "invoke",
        new StubParam[] {
          new StubParam(Object.class, "impl"),
          new StubParam(RuntimeClassNames.REQUEST_STREAM, "in"),
          new StubParam(RuntimeClassNames.CODEC, "codec"),
          new StubParam(RuntimeClassNames.CALL_CONTEXT, "ctx")
        },
        ArrayTypeName.of(byte.class));
  }

  private static MethodSpec buildBidiStreamStub(ServiceModel svc, MethodModel method) {
    return streamStubMethod(
        "invoke",
        new StubParam[] {
          new StubParam(Object.class, "impl"),
          new StubParam(RuntimeClassNames.REQUEST_STREAM, "in"),
          new StubParam(RuntimeClassNames.CODEC, "codec"),
          new StubParam(RuntimeClassNames.CALL_CONTEXT, "ctx"),
          new StubParam(RuntimeClassNames.RESPONSE_STREAM, "out")
        },
        TypeName.VOID);
  }

  private record StubParam(Object type, String name) {}

  private static MethodSpec streamStubMethod(String name, StubParam[] params, TypeName returns) {
    MethodSpec.Builder b =
        MethodSpec.methodBuilder(name)
            .addAnnotation(Override.class)
            .addModifiers(Modifier.PUBLIC)
            .returns(returns)
            .addException(Exception.class);
    for (StubParam p : params) {
      if (p.type() instanceof TypeName tn) {
        b.addParameter(tn, p.name());
      } else if (p.type() instanceof Class<?> c) {
        b.addParameter(c, p.name());
      }
    }
    b.addStatement(
        "throw new $T($S)",
        UnsupportedOperationException.class,
        "streaming dispatcher body not yet emitted — populated by Commit D (codegen-ksp Flow bridging)");
    return b.build();
  }

  /**
   * Resolve the Fory XLANG tag for {@code type} used by this service. When the caller supplied a
   * {@code @WireType}-derived tag via {@link ServiceModel#wireTypeTags()}, use it verbatim so Java
   * matches Python's / TS's on-wire identity for the same logical type. Otherwise derive from the
   * Java package + simple name.
   */
  private static String foryTagFor(TypeName type, ServiceModel svc) {
    if (type == null) {
      return "";
    }
    String explicit = svc.wireTypeTags().get(type.toString());
    if (explicit != null && !explicit.isEmpty()) {
      return explicit;
    }
    if (type instanceof ClassName cn) {
      return cn.packageName() + "/" + cn.simpleName();
    }
    return type.toString();
  }

  private static boolean isJavaLangType(ClassName cn) {
    return "java.lang".equals(cn.packageName());
  }
}
