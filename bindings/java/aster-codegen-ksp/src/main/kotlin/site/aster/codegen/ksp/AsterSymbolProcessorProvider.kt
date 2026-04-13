package site.aster.codegen.ksp

import com.google.devtools.ksp.processing.CodeGenerator
import com.google.devtools.ksp.processing.Dependencies
import com.google.devtools.ksp.processing.KSPLogger
import com.google.devtools.ksp.processing.Resolver
import com.google.devtools.ksp.processing.SymbolProcessor
import com.google.devtools.ksp.processing.SymbolProcessorEnvironment
import com.google.devtools.ksp.processing.SymbolProcessorProvider
import com.google.devtools.ksp.symbol.KSAnnotated
import com.google.devtools.ksp.symbol.KSClassDeclaration
import site.aster.codegen.core.model.ServiceModel

/**
 * KSP entry point for Aster. Scans `@site.aster.annotations.Service`-annotated Kotlin (and Java)
 * classes, builds a [ServiceModel] via [KotlinModelBuilder], then hands it to
 * [KotlinDispatcherEmitter] to produce the Kotlin dispatcher + inline request records.
 *
 * End-to-end validation happens in Commit G's Kotlin MissionControl sample — this module itself
 * ships without unit tests because KSP-in-Maven test harnesses are clunky and the sample build
 * exercises the full pipeline for free.
 */
class AsterSymbolProcessorProvider : SymbolProcessorProvider {
  override fun create(environment: SymbolProcessorEnvironment): SymbolProcessor =
    AsterSymbolProcessor(environment.codeGenerator, environment.logger)
}

private class AsterSymbolProcessor(
  private val codeGenerator: CodeGenerator,
  private val logger: KSPLogger,
) : SymbolProcessor {

  private val generatedDispatchers = linkedSetOf<String>()
  private var servicesFileWritten = false

  override fun process(resolver: Resolver): List<KSAnnotated> {
    val serviceAnnotation = "site.aster.annotations.Service"
    val annotated = resolver
      .getSymbolsWithAnnotation(serviceAnnotation)
      .filterIsInstance<KSClassDeclaration>()
      .toList()

    for (svcClass in annotated) {
      try {
        val model = KotlinModelBuilder(logger).build(svcClass) ?: continue
        KotlinDispatcherEmitter.emit(model, svcClass, codeGenerator)
        KotlinInlineRecordEmitter.emit(model, svcClass, codeGenerator)
        val dispatcherFqn = "${model.implClass().packageName()}." +
          "${model.implClass().simpleName()}\$AsterDispatcher"
        generatedDispatchers.add(dispatcherFqn)
      } catch (t: Throwable) {
        logger.error("Aster codegen failed for $svcClass: ${t.message}", svcClass)
      }
    }
    return emptyList()
  }

  override fun finish() {
    if (generatedDispatchers.isEmpty() || servicesFileWritten) {
      return
    }
    servicesFileWritten = true
    try {
      // KSP 1.0.24+ exposes createNewFileByPath for arbitrary resource paths. Gradle's KSP
      // integration copies files under META-INF/services/** into the output JAR automatically.
      codeGenerator.createNewFileByPath(
        dependencies = Dependencies(aggregating = true),
        path = "META-INF/services/site.aster.server.spi.ServiceDispatcher",
        extensionName = "",
      ).use { out ->
        val writer = out.bufferedWriter()
        for (cls in generatedDispatchers) {
          writer.write(cls)
          writer.newLine()
        }
        writer.flush()
      }
    } catch (t: Throwable) {
      // Multi-round processing surfaces FileAlreadyExistsException; harmless if the prior
      // write already captured the same dispatcher set.
      logger.warn("Could not write ServiceDispatcher services file: ${t.message}")
    }
  }
}
