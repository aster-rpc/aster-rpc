package site.aster.codegen.core.model;

import com.palantir.javapoet.ClassName;
import java.util.List;
import java.util.Map;
import site.aster.annotations.Scope;

/**
 * Language-neutral description of an annotated service class. The emitter produces one
 * ServiceDispatcher + zero or more synthesized request records per instance of this model.
 *
 * <p>{@code description} and {@code tags} are non-canonical. They flow into the published {@code
 * ContractManifest} JSON and surface in MCP / shell views, but do not affect the contract identity
 * hash.
 *
 * <p>{@code wireTypeTags} maps from a {@link com.palantir.javapoet.TypeName#toString() TypeName
 * string} (the same key the emitter uses for deduping registrations) to an explicit Fory XLANG tag
 * sourced from {@code @WireType}. Types not present in this map fall back to derivation from the
 * Java package + simple name.
 */
public record ServiceModel(
    String name,
    int version,
    Scope scope,
    ClassName implClass,
    List<MethodModel> methods,
    String description,
    List<String> tags,
    Map<String, String> wireTypeTags,
    RequiresSpec requires) {

  public ServiceModel {
    methods = List.copyOf(methods);
    description = description == null ? "" : description;
    tags = tags == null ? List.of() : List.copyOf(tags);
    wireTypeTags = wireTypeTags == null ? Map.of() : Map.copyOf(wireTypeTags);
  }

  /** Legacy constructor (pre-metadata). */
  public ServiceModel(
      String name, int version, Scope scope, ClassName implClass, List<MethodModel> methods) {
    this(name, version, scope, implClass, methods, "", List.of(), Map.of(), null);
  }

  /** Legacy constructor (pre-wire-tag). */
  public ServiceModel(
      String name,
      int version,
      Scope scope,
      ClassName implClass,
      List<MethodModel> methods,
      String description,
      List<String> tags) {
    this(name, version, scope, implClass, methods, description, tags, Map.of(), null);
  }

  /** Legacy constructor (pre-{@code requires}). */
  public ServiceModel(
      String name,
      int version,
      Scope scope,
      ClassName implClass,
      List<MethodModel> methods,
      String description,
      List<String> tags,
      Map<String, String> wireTypeTags) {
    this(name, version, scope, implClass, methods, description, tags, wireTypeTags, null);
  }
}
