package site.aster.codegen.core.model;

import com.palantir.javapoet.ClassName;
import java.util.List;
import site.aster.annotations.Scope;

/**
 * Language-neutral description of an annotated service class. The emitter produces one
 * ServiceDispatcher + zero or more synthesized request records per instance of this model.
 *
 * <p>{@code description} and {@code tags} are non-canonical. They flow into the published {@code
 * ContractManifest} JSON and surface in MCP / shell views, but do not affect the contract identity
 * hash.
 */
public record ServiceModel(
    String name,
    int version,
    Scope scope,
    ClassName implClass,
    List<MethodModel> methods,
    String description,
    List<String> tags) {

  public ServiceModel {
    methods = List.copyOf(methods);
    description = description == null ? "" : description;
    tags = tags == null ? List.of() : List.copyOf(tags);
  }

  /** Legacy constructor for callers not yet supplying metadata. */
  public ServiceModel(
      String name, int version, Scope scope, ClassName implClass, List<MethodModel> methods) {
    this(name, version, scope, implClass, methods, "", List.of());
  }
}
