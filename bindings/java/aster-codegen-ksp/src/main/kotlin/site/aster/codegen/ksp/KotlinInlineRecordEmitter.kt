package site.aster.codegen.ksp

import com.google.devtools.ksp.processing.CodeGenerator
import com.google.devtools.ksp.processing.Dependencies
import com.google.devtools.ksp.symbol.KSClassDeclaration
import site.aster.codegen.core.emit.RequestRecordEmitter
import site.aster.codegen.core.model.ServiceModel

/**
 * Day-0 skeleton: delegates to the Java [RequestRecordEmitter]. Synthesized inline request
 * records land as `.java` files in the KSP output tree, compiled by javac alongside the
 * Kotlin sources. Commit G switches to native Kotlin data classes via KotlinPoet.
 */
internal object KotlinInlineRecordEmitter {

  fun emit(model: ServiceModel, origin: KSClassDeclaration, codeGenerator: CodeGenerator) {
    val javaFiles = RequestRecordEmitter.emit(model)
    for (file in javaFiles) {
      val path = file.packageName().replace('.', '/') + "/" + file.typeSpec().name() + ".java"
      codeGenerator.createNewFileByPath(
        dependencies = Dependencies(aggregating = false, origin.containingFile!!),
        path = path,
        extensionName = "",
      ).use { out ->
        val writer = out.bufferedWriter()
        writer.write(file.toString())
        writer.flush()
      }
    }
  }
}
