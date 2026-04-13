package site.aster.codegen.apt;

import java.util.Set;
import javax.annotation.processing.AbstractProcessor;
import javax.annotation.processing.RoundEnvironment;
import javax.annotation.processing.SupportedAnnotationTypes;
import javax.annotation.processing.SupportedSourceVersion;
import javax.lang.model.SourceVersion;
import javax.lang.model.element.TypeElement;

@SupportedAnnotationTypes({
  "site.aster.annotations.Service",
  "site.aster.annotations.Rpc",
  "site.aster.annotations.ServerStream",
  "site.aster.annotations.ClientStream",
  "site.aster.annotations.BidiStream"
})
@SupportedSourceVersion(SourceVersion.RELEASE_25)
public final class AsterAnnotationProcessor extends AbstractProcessor {
  @Override
  public boolean process(Set<? extends TypeElement> annotations, RoundEnvironment roundEnv) {
    return false;
  }
}
