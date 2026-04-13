package site.aster.codegen.ksp

import com.google.devtools.ksp.processing.CodeGenerator
import com.google.devtools.ksp.processing.Dependencies
import com.google.devtools.ksp.symbol.KSClassDeclaration
import site.aster.codegen.core.emit.DispatcherEmitter
import site.aster.codegen.core.model.ServiceModel

/**
 * Day-0 skeleton emitter. Delegates to the Java-emitting [DispatcherEmitter] and writes the
 * produced Java source into KSP's generated-sources tree. Kotlin will pick up the Java file on
 * its classpath — KSP multi-language output is supported.
 *
 * Commit G replaces this with a KotlinPoet-based emitter that produces a native Kotlin
 * dispatcher with Flow collection and suspend-fun bridging. Until then, this skeleton is
 * functionally equivalent to the APT output and unblocks Commit E's AsterServer work.
 */
internal object KotlinDispatcherEmitter {

  fun emit(model: ServiceModel, origin: KSClassDeclaration, codeGenerator: CodeGenerator) {
    val javaFile = DispatcherEmitter.emit(model)
    val path =
      javaFile.packageName().replace('.', '/') + "/" + javaFile.typeSpec().name() + ".java"

    codeGenerator.createNewFileByPath(
      dependencies = Dependencies(aggregating = false, origin.containingFile!!),
      path = path,
      extensionName = "",
    ).use { out ->
      val writer = out.bufferedWriter()
      writer.write(javaFile.toString())
      writer.flush()
    }
  }
}
