package site.aster.codegen.apt;

import com.palantir.javapoet.ClassName;
import com.palantir.javapoet.JavaFile;
import java.io.IOException;
import java.io.Writer;
import java.util.ArrayList;
import java.util.LinkedHashSet;
import java.util.List;
import java.util.Set;
import javax.annotation.processing.AbstractProcessor;
import javax.annotation.processing.Filer;
import javax.annotation.processing.Messager;
import javax.annotation.processing.ProcessingEnvironment;
import javax.annotation.processing.RoundEnvironment;
import javax.annotation.processing.SupportedAnnotationTypes;
import javax.annotation.processing.SupportedSourceVersion;
import javax.lang.model.SourceVersion;
import javax.lang.model.element.Element;
import javax.lang.model.element.TypeElement;
import javax.lang.model.util.Elements;
import javax.tools.Diagnostic;
import javax.tools.FileObject;
import javax.tools.StandardLocation;
import site.aster.codegen.core.emit.DispatcherEmitter;
import site.aster.codegen.core.emit.NameConventions;
import site.aster.codegen.core.emit.RequestRecordEmitter;
import site.aster.codegen.core.model.ServiceModel;

/**
 * Aster annotation processor. Scans {@code @Service}-annotated classes, builds a {@link
 * ServiceModel} for each, and emits the ServiceDispatcher + inline request records via {@code
 * aster-codegen-core}. Also writes a {@code
 * META-INF/services/site.aster.server.spi.ServiceDispatcher} entry so the generated dispatchers are
 * discoverable at runtime via ServiceLoader.
 */
@SupportedAnnotationTypes({
  "site.aster.annotations.Service",
  "site.aster.annotations.Rpc",
  "site.aster.annotations.ServerStream",
  "site.aster.annotations.ClientStream",
  "site.aster.annotations.BidiStream"
})
@SupportedSourceVersion(SourceVersion.RELEASE_25)
public final class AsterAnnotationProcessor extends AbstractProcessor {

  private static final String SERVICE_DISPATCHER_FQN = "site.aster.server.spi.ServiceDispatcher";
  private static final String SERVICE_FILE = "META-INF/services/" + SERVICE_DISPATCHER_FQN;

  private Messager messager;
  private Filer filer;
  private Elements elements;
  private ModelBuilder modelBuilder;
  private final LinkedHashSet<String> generatedDispatchers = new LinkedHashSet<>();

  @Override
  public synchronized void init(ProcessingEnvironment processingEnv) {
    super.init(processingEnv);
    this.messager = processingEnv.getMessager();
    this.filer = processingEnv.getFiler();
    this.elements = processingEnv.getElementUtils();
    this.modelBuilder = new ModelBuilder(messager, elements);
  }

  @Override
  public boolean process(Set<? extends TypeElement> annotations, RoundEnvironment roundEnv) {
    TypeElement serviceAnnotation = elements.getTypeElement("site.aster.annotations.Service");
    if (serviceAnnotation == null) {
      return false;
    }

    List<TypeElement> serviceTypes = new ArrayList<>();
    for (Element e : roundEnv.getElementsAnnotatedWith(serviceAnnotation)) {
      if (e instanceof TypeElement te) {
        serviceTypes.add(te);
      }
    }

    for (TypeElement svcType : serviceTypes) {
      ServiceModel model = modelBuilder.build(svcType);
      if (model == null) {
        continue;
      }
      try {
        // Emit the dispatcher class
        JavaFile dispatcher = DispatcherEmitter.emit(model);
        dispatcher.writeTo(filer);

        // Emit any synthesized inline request records
        for (JavaFile record : RequestRecordEmitter.emit(model)) {
          record.writeTo(filer);
        }

        ClassName dispatcherName = NameConventions.dispatcherClassName(model);
        generatedDispatchers.add(dispatcherName.packageName() + "." + dispatcherName.simpleName());
      } catch (IOException e) {
        messager.printMessage(
            Diagnostic.Kind.ERROR,
            "Failed to write generated sources for " + svcType + ": " + e.getMessage(),
            svcType);
      }
    }

    if (roundEnv.processingOver() && !generatedDispatchers.isEmpty()) {
      writeServiceFile();
    }

    return true;
  }

  private void writeServiceFile() {
    try {
      FileObject existing = null;
      try {
        existing = filer.getResource(StandardLocation.CLASS_OUTPUT, "", SERVICE_FILE);
      } catch (IOException ignored) {
        // No previous file; normal on first run.
      }
      LinkedHashSet<String> all = new LinkedHashSet<>();
      if (existing != null) {
        try {
          all.addAll(List.of(existing.getCharContent(true).toString().split("\\R")));
        } catch (IOException ignored) {
          // Fall through; we'll write a fresh file.
        }
      }
      all.addAll(generatedDispatchers);
      all.removeIf(String::isBlank);

      FileObject out = filer.createResource(StandardLocation.CLASS_OUTPUT, "", SERVICE_FILE);
      try (Writer w = out.openWriter()) {
        for (String cls : all) {
          w.write(cls);
          w.write('\n');
        }
      }
    } catch (IOException e) {
      messager.printMessage(
          Diagnostic.Kind.ERROR, "Failed to write " + SERVICE_FILE + ": " + e.getMessage());
    }
  }
}
