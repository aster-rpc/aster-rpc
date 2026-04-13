package site.aster.codegen.core.model;

import com.palantir.javapoet.TypeName;
import java.util.List;

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
    boolean isSuspend) {

  public MethodModel {
    inlineParams = List.copyOf(inlineParams);
  }
}
