package site.aster.codegen.core.model;

import com.palantir.javapoet.ClassName;
import java.util.List;
import site.aster.annotations.Scope;

/**
 * Language-neutral description of an annotated service class. The emitter produces one
 * ServiceDispatcher + zero or more synthesized request records per instance of this model.
 */
public record ServiceModel(
    String name, int version, Scope scope, ClassName implClass, List<MethodModel> methods) {

  public ServiceModel {
    methods = List.copyOf(methods);
  }
}
