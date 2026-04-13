package site.aster.codegen.apt;

import com.palantir.javapoet.ClassName;
import com.palantir.javapoet.TypeName;
import java.util.ArrayList;
import java.util.List;
import javax.annotation.processing.Messager;
import javax.lang.model.element.Element;
import javax.lang.model.element.ElementKind;
import javax.lang.model.element.ExecutableElement;
import javax.lang.model.element.Modifier;
import javax.lang.model.element.PackageElement;
import javax.lang.model.element.TypeElement;
import javax.lang.model.element.VariableElement;
import javax.lang.model.type.ArrayType;
import javax.lang.model.type.DeclaredType;
import javax.lang.model.type.PrimitiveType;
import javax.lang.model.type.TypeKind;
import javax.lang.model.type.TypeMirror;
import javax.lang.model.util.Elements;
import javax.tools.Diagnostic;
import site.aster.annotations.BidiStream;
import site.aster.annotations.ClientStream;
import site.aster.annotations.Rpc;
import site.aster.annotations.ServerStream;
import site.aster.annotations.Service;
import site.aster.codegen.core.model.MethodModel;
import site.aster.codegen.core.model.ParamModel;
import site.aster.codegen.core.model.RequestStyle;
import site.aster.codegen.core.model.ServiceModel;
import site.aster.codegen.core.model.StreamingKind;

/**
 * Walks a {@code @Service}-annotated {@link TypeElement} and produces a {@link ServiceModel} that
 * {@code aster-codegen-core} can emit. Mode 1/Mode 2 detection matches {@code
 * bindings/python/aster/inline_params.py}: one {@code @WireType}-style param ⇒ EXPLICIT, anything
 * else ⇒ INLINE.
 *
 * <p>Methods without one of the four method-level annotations ({@code @Rpc}, {@code @ServerStream},
 * {@code @ClientStream}, {@code @BidiStream}) are skipped. Fatal errors (e.g. an annotation on a
 * non-public method) are reported via {@link Messager} and cause the method to be dropped.
 */
final class ModelBuilder {

  private static final String CALL_CONTEXT_FQN = "site.aster.interceptors.CallContext";

  private final Messager messager;
  private final Elements elements;

  ModelBuilder(Messager messager, Elements elements) {
    this.messager = messager;
    this.elements = elements;
  }

  ServiceModel build(TypeElement serviceType) {
    Service serviceAnn = serviceType.getAnnotation(Service.class);
    if (serviceAnn == null) {
      messager.printMessage(
          Diagnostic.Kind.ERROR, "@Service annotation missing on " + serviceType, serviceType);
      return null;
    }

    PackageElement pkg = elements.getPackageOf(serviceType);
    ClassName implClass =
        ClassName.get(pkg.getQualifiedName().toString(), serviceType.getSimpleName().toString());

    List<MethodModel> methods = new ArrayList<>();
    for (Element member : serviceType.getEnclosedElements()) {
      if (member.getKind() != ElementKind.METHOD) {
        continue;
      }
      ExecutableElement exec = (ExecutableElement) member;
      MethodModel mm = classifyMethod(exec);
      if (mm != null) {
        methods.add(mm);
      }
    }

    return new ServiceModel(
        serviceAnn.name(), serviceAnn.version(), serviceAnn.scoped(), implClass, methods);
  }

  private MethodModel classifyMethod(ExecutableElement exec) {
    StreamingKind streaming = streamingKindFor(exec);
    if (streaming == null) {
      return null; // Not an RPC method.
    }
    if (!exec.getModifiers().contains(Modifier.PUBLIC)) {
      messager.printMessage(
          Diagnostic.Kind.ERROR, "RPC methods must be public: " + exec.getSimpleName(), exec);
      return null;
    }

    String wireName = wireNameFor(exec);
    boolean hasContextParam = false;
    List<ParamModel> nonCtxParams = new ArrayList<>();
    TypeName explicitRequest = null;

    for (VariableElement p : exec.getParameters()) {
      TypeMirror ptype = p.asType();
      if (isCallContext(ptype)) {
        if (hasContextParam) {
          messager.printMessage(
              Diagnostic.Kind.ERROR,
              "RPC method may declare at most one CallContext parameter: " + exec.getSimpleName(),
              p);
          return null;
        }
        hasContextParam = true;
        continue;
      }
      TypeName tn = toTypeName(ptype);
      if (tn == null) {
        messager.printMessage(
            Diagnostic.Kind.ERROR, "Could not resolve parameter type for " + p.getSimpleName(), p);
        return null;
      }
      nonCtxParams.add(new ParamModel(p.getSimpleName().toString(), tn));
    }

    RequestStyle style;
    List<ParamModel> inlineParams;
    if (nonCtxParams.size() == 1 && looksLikeWireType(nonCtxParams.get(0).type())) {
      style = RequestStyle.EXPLICIT;
      explicitRequest = nonCtxParams.get(0).type();
      inlineParams = List.of();
    } else {
      style = RequestStyle.INLINE;
      inlineParams = nonCtxParams;
    }

    TypeName responseType = toTypeName(exec.getReturnType());
    if (responseType == null && exec.getReturnType().getKind() != TypeKind.VOID) {
      messager.printMessage(
          Diagnostic.Kind.ERROR, "Could not resolve return type for " + exec.getSimpleName(), exec);
      return null;
    }

    return new MethodModel(
        exec.getSimpleName().toString(),
        wireName,
        streaming,
        style,
        inlineParams,
        explicitRequest,
        responseType,
        hasContextParam,
        false,
        false);
  }

  private static StreamingKind streamingKindFor(ExecutableElement exec) {
    if (exec.getAnnotation(Rpc.class) != null) {
      return StreamingKind.UNARY;
    }
    if (exec.getAnnotation(ServerStream.class) != null) {
      return StreamingKind.SERVER_STREAM;
    }
    if (exec.getAnnotation(ClientStream.class) != null) {
      return StreamingKind.CLIENT_STREAM;
    }
    if (exec.getAnnotation(BidiStream.class) != null) {
      return StreamingKind.BIDI_STREAM;
    }
    return null;
  }

  private static String wireNameFor(ExecutableElement exec) {
    Rpc rpc = exec.getAnnotation(Rpc.class);
    if (rpc != null && !rpc.name().isEmpty()) {
      return rpc.name();
    }
    ServerStream ss = exec.getAnnotation(ServerStream.class);
    if (ss != null && !ss.name().isEmpty()) {
      return ss.name();
    }
    ClientStream cs = exec.getAnnotation(ClientStream.class);
    if (cs != null && !cs.name().isEmpty()) {
      return cs.name();
    }
    BidiStream bs = exec.getAnnotation(BidiStream.class);
    if (bs != null && !bs.name().isEmpty()) {
      return bs.name();
    }
    return exec.getSimpleName().toString();
  }

  private static boolean isCallContext(TypeMirror t) {
    if (t.getKind() != TypeKind.DECLARED) {
      return false;
    }
    DeclaredType dt = (DeclaredType) t;
    TypeElement te = (TypeElement) dt.asElement();
    return CALL_CONTEXT_FQN.contentEquals(te.getQualifiedName());
  }

  /**
   * Heuristic for Mode 1 vs Mode 2. Mirrors Python's {@code _looks_inline}: a type is considered
   * "inline-friendly" if it's a primitive, boxed primitive, or {@link String}. Any declared type
   * that is NOT one of those is treated as an explicit wire type candidate. Java has no
   * {@code @wire_type} runtime marker yet, so we classify by the Java-type category.
   */
  private static boolean looksLikeWireType(TypeName tn) {
    if (tn instanceof ClassName cn) {
      String pkg = cn.packageName();
      if ("java.lang".equals(pkg)) {
        String simple = cn.simpleName();
        return !(simple.equals("String")
            || simple.equals("Integer")
            || simple.equals("Long")
            || simple.equals("Short")
            || simple.equals("Byte")
            || simple.equals("Float")
            || simple.equals("Double")
            || simple.equals("Boolean")
            || simple.equals("Character"));
      }
      return true;
    }
    return false;
  }

  private static TypeName toTypeName(TypeMirror t) {
    switch (t.getKind()) {
      case VOID:
        return null;
      case BOOLEAN:
      case BYTE:
      case SHORT:
      case INT:
      case LONG:
      case CHAR:
      case FLOAT:
      case DOUBLE:
        return primitiveBox((PrimitiveType) t);
      case DECLARED:
        DeclaredType dt = (DeclaredType) t;
        TypeElement te = (TypeElement) dt.asElement();
        return ClassName.get(te);
      case ARRAY:
        TypeMirror comp = ((ArrayType) t).getComponentType();
        TypeName compName = toTypeName(comp);
        return compName == null ? null : com.palantir.javapoet.ArrayTypeName.of(compName);
      default:
        return null;
    }
  }

  private static TypeName primitiveBox(PrimitiveType t) {
    // We box primitives in the model so inline request records use boxed types, matching
    // Python's handling of default-less primitive fields under Fory xlang serialization.
    return switch (t.getKind()) {
      case BOOLEAN -> ClassName.get(Boolean.class);
      case BYTE -> ClassName.get(Byte.class);
      case SHORT -> ClassName.get(Short.class);
      case INT -> ClassName.get(Integer.class);
      case LONG -> ClassName.get(Long.class);
      case CHAR -> ClassName.get(Character.class);
      case FLOAT -> ClassName.get(Float.class);
      case DOUBLE -> ClassName.get(Double.class);
      default -> throw new AssertionError("non-primitive: " + t.getKind());
    };
  }
}
