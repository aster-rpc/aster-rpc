package site.aster.codegen.core.model;

import com.palantir.javapoet.TypeName;
import java.util.List;
import java.util.Map;

/**
 * Language-neutral description of one annotated method. Produced by {@code aster-codegen-apt} (from
 * {@code ExecutableElement}) or {@code aster-codegen-ksp} (from {@code KSFunctionDeclaration}).
 *
 * <p>For {@link RequestStyle#EXPLICIT}, {@code requestType} is the user's {@code @WireType} class
 * and {@code inlineParams} is empty. For {@link RequestStyle#INLINE}, {@code requestType} is {@code
 * null} and {@code inlineParams} holds the positional parameters; the emitter synthesizes a request
 * record for it.
 *
 * <p>{@code isSuspend} is only meaningful for Kotlin sources; KSP sets it, APT always leaves it
 * false. The emitter routes suspend/Flow bodies through kotlinx-coroutines-jdk8 bridges.
 *
 * <p>{@code description}, {@code tags}, {@code deprecated}, and {@code fieldMetadata} are
 * non-canonical: they flow into the manifest JSON but do not affect the contract identity hash.
 * {@code fieldMetadata} keys are wire field names (matching record component names for explicit
 * requests or inline param names for Mode 2).
 */
public record MethodModel(
    String name,
    String wireName,
    StreamingKind streaming,
    RequestStyle requestStyle,
    List<ParamModel> inlineParams,
    TypeName requestType,
    TypeName responseType,
    boolean hasContextParam,
    boolean idempotent,
    boolean isSuspend,
    String description,
    List<String> tags,
    boolean deprecated,
    Map<String, FieldModel> fieldMetadata) {

  public MethodModel {
    inlineParams = List.copyOf(inlineParams);
    description = description == null ? "" : description;
    tags = tags == null ? List.of() : List.copyOf(tags);
    fieldMetadata = fieldMetadata == null ? Map.of() : Map.copyOf(fieldMetadata);
  }

  /** Legacy constructor for callers not yet supplying metadata. */
  public MethodModel(
      String name,
      String wireName,
      StreamingKind streaming,
      RequestStyle requestStyle,
      List<ParamModel> inlineParams,
      TypeName requestType,
      TypeName responseType,
      boolean hasContextParam,
      boolean idempotent,
      boolean isSuspend) {
    this(
        name,
        wireName,
        streaming,
        requestStyle,
        inlineParams,
        requestType,
        responseType,
        hasContextParam,
        idempotent,
        isSuspend,
        "",
        List.of(),
        false,
        Map.of());
  }
}
