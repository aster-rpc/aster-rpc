package site.aster.codegen.core.emit;

import com.palantir.javapoet.AnnotationSpec;
import com.palantir.javapoet.ClassName;
import com.palantir.javapoet.JavaFile;
import com.palantir.javapoet.MethodSpec;
import com.palantir.javapoet.ParameterSpec;
import com.palantir.javapoet.TypeSpec;
import java.util.ArrayList;
import java.util.List;
import javax.annotation.processing.Generated;
import javax.lang.model.element.Modifier;
import site.aster.codegen.core.model.MethodModel;
import site.aster.codegen.core.model.ParamModel;
import site.aster.codegen.core.model.RequestStyle;
import site.aster.codegen.core.model.ServiceModel;

/**
 * Emits {@code {ServiceSimpleName}_{MethodPascalCase}Request} records for {@link
 * RequestStyle#INLINE} methods. One record per INLINE method. Empty-parameter methods still get a
 * record (zero components) so the wire-level round-trip is uniform.
 *
 * <p>The records are plain Java records with one component per inline parameter. They're not
 * annotated with any framework marker — registration with Fory happens at dispatcher construction
 * time via {@code ServiceDispatcher.registerTypes(Fory)}.
 */
public final class RequestRecordEmitter {

  private RequestRecordEmitter() {}

  public static List<JavaFile> emit(ServiceModel svc) {
    List<JavaFile> out = new ArrayList<>();
    for (MethodModel m : svc.methods()) {
      if (m.requestStyle() == RequestStyle.INLINE) {
        out.add(emitOne(svc, m));
      }
    }
    return out;
  }

  private static JavaFile emitOne(ServiceModel svc, MethodModel method) {
    ClassName recordName = NameConventions.inlineRequestClassName(svc, method);
    List<ParameterSpec> components = new ArrayList<>();
    for (ParamModel p : method.inlineParams()) {
      components.add(ParameterSpec.builder(p.type(), p.name()).build());
    }
    MethodSpec canonicalCtor = MethodSpec.compactConstructorBuilder().build();
    TypeSpec.Builder typeBuilder =
        TypeSpec.recordBuilder(recordName.simpleName())
            .addModifiers(Modifier.PUBLIC)
            .recordConstructor(
                MethodSpec.constructorBuilder()
                    .addModifiers(Modifier.PUBLIC)
                    .addParameters(components)
                    .build())
            .addMethod(canonicalCtor)
            .addAnnotation(
                AnnotationSpec.builder(Generated.class)
                    .addMember("value", "$S", "site.aster.codegen")
                    .build());

    return JavaFile.builder(recordName.packageName(), typeBuilder.build())
        .skipJavaLangImports(true)
        .indent("  ")
        .build();
  }
}
