package site.aster.codegen.core.model;

import com.palantir.javapoet.TypeName;

/**
 * One positional parameter on an inline ({@link RequestStyle#INLINE}) method. Captured once by the
 * processor at build time and consumed by emitters.
 *
 * @param name parameter name as declared in the user source
 * @param type Java type reference used for synthesizing the {@code {Method}Request} record and for
 *     unpacking in the generated dispatcher
 */
public record ParamModel(String name, TypeName type) {}
