package site.aster.codegen.ksp

import com.google.devtools.ksp.processing.SymbolProcessor
import com.google.devtools.ksp.processing.SymbolProcessorEnvironment
import com.google.devtools.ksp.processing.SymbolProcessorProvider
import com.google.devtools.ksp.processing.Resolver

class AsterSymbolProcessorProvider : SymbolProcessorProvider {
  override fun create(environment: SymbolProcessorEnvironment): SymbolProcessor =
    AsterSymbolProcessor()
}

private class AsterSymbolProcessor : SymbolProcessor {
  override fun process(resolver: Resolver): List<com.google.devtools.ksp.symbol.KSAnnotated> = emptyList()
}
