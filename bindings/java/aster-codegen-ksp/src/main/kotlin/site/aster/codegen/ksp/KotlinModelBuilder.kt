package site.aster.codegen.ksp

import com.google.devtools.ksp.KspExperimental
import com.google.devtools.ksp.getAnnotationsByType
import com.google.devtools.ksp.processing.KSPLogger
import com.google.devtools.ksp.symbol.ClassKind
import com.google.devtools.ksp.symbol.KSClassDeclaration
import com.google.devtools.ksp.symbol.KSFunctionDeclaration
import com.google.devtools.ksp.symbol.KSType
import com.google.devtools.ksp.symbol.Modifier
import com.palantir.javapoet.ClassName
import com.palantir.javapoet.TypeName
import site.aster.annotations.BidiStream
import site.aster.annotations.ClientStream
import site.aster.annotations.Description
import site.aster.annotations.Rpc
import site.aster.annotations.ServerStream
import site.aster.annotations.Service
import site.aster.codegen.core.model.FieldModel
import site.aster.codegen.core.model.MethodModel
import site.aster.codegen.core.model.ParamModel
import site.aster.codegen.core.model.RequestStyle
import site.aster.codegen.core.model.ServiceModel
import site.aster.codegen.core.model.StreamingKind

/**
 * Walks a `@Service`-annotated Kotlin [KSClassDeclaration] and produces a language-neutral
 * [ServiceModel] the emitters can consume. Mirrors the APT classifier in
 * `site.aster.codegen.apt.ModelBuilder` so a mixed Java + Kotlin build produces consistent
 * dispatchers.
 *
 * This is the Day-0 skeleton: `suspend fun` is recorded on [MethodModel.isSuspend] but Kotlin-
 * specific streaming shapes (`Flow<T>` returns, `Flow<R>` params) are NOT yet routed to a
 * streaming kind by inspection of the KSType generic arguments. That wiring lands alongside the
 * KotlinPoet Flow-bridging emitter in Commit G, where we can iterate against a real sample.
 */
@OptIn(KspExperimental::class)
internal class KotlinModelBuilder(private val logger: KSPLogger) {

  fun build(svc: KSClassDeclaration): ServiceModel? {
    if (svc.classKind != ClassKind.CLASS && svc.classKind != ClassKind.OBJECT) {
      logger.error(
        "@Service must be applied to a class or object: ${svc.qualifiedName?.asString()}", svc,
      )
      return null
    }
    val serviceAnn = svc.getAnnotationsByType(Service::class).firstOrNull() ?: run {
      logger.error("Missing @Service annotation on ${svc.qualifiedName?.asString()}", svc)
      return null
    }

    val pkg = svc.packageName.asString()
    val implClass = ClassName.get(pkg, svc.simpleName.asString())

    val methods = svc.declarations
      .filterIsInstance<KSFunctionDeclaration>()
      .mapNotNull { classifyFunction(it) }
      .toList()

    val description = serviceAnn.description.ifEmpty { firstParagraph(svc.docString) }
    val tags = serviceAnn.tags.toList()

    return ServiceModel(
      serviceAnn.name,
      serviceAnn.version,
      serviceAnn.scoped,
      implClass,
      methods,
      description,
      tags,
    )
  }

  private fun classifyFunction(fn: KSFunctionDeclaration): MethodModel? {
    val streaming = streamingKindFor(fn) ?: return null
    if (Modifier.PRIVATE in fn.modifiers || Modifier.PROTECTED in fn.modifiers) {
      logger.error("RPC functions must be public: ${fn.simpleName.asString()}", fn)
      return null
    }
    val wireName = wireNameFor(fn)
    val isSuspend = Modifier.SUSPEND in fn.modifiers

    var hasContextParam = false
    val nonCtxParams = mutableListOf<ParamModel>()

    for (p in fn.parameters) {
      val paramType = p.type.resolve()
      if (isCallContext(paramType)) {
        if (hasContextParam) {
          logger.error(
            "At most one CallContext parameter is allowed: ${fn.simpleName.asString()}", p,
          )
          return null
        }
        hasContextParam = true
        continue
      }
      val tn = ksTypeToTypeName(paramType) ?: run {
        logger.error(
          "Could not resolve parameter type ${paramType.declaration.qualifiedName?.asString()}", p,
        )
        return null
      }
      val paramAnn = p.getAnnotationsByType(Description::class).firstOrNull()
      val paramDesc = paramAnn?.value.orEmpty()
      val paramTags = paramAnn?.tags?.toList().orEmpty()
      nonCtxParams.add(ParamModel(p.name?.asString() ?: "arg", tn, paramDesc, paramTags))
    }

    val style: RequestStyle
    val inlineParams: List<ParamModel>
    val requestType: TypeName?
    if (nonCtxParams.size == 1 && looksLikeWireType(nonCtxParams[0].type())) {
      style = RequestStyle.EXPLICIT
      requestType = nonCtxParams[0].type()
      inlineParams = emptyList()
    } else {
      style = RequestStyle.INLINE
      requestType = null
      inlineParams = nonCtxParams
    }

    val responseType = fn.returnType?.resolve()?.let { ksTypeToTypeName(it) }

    val meta = readMethodMeta(fn)
    val description = meta.description.ifEmpty { firstParagraph(fn.docString) }
    val fieldMeta: Map<String, FieldModel> = if (style == RequestStyle.INLINE) {
      inlineParams
        .filter { it.description().isNotEmpty() || it.tags().isNotEmpty() }
        .associate { it.name() to FieldModel(it.name(), it.description(), it.tags()) }
    } else {
      emptyMap()
    }

    return MethodModel(
      fn.simpleName.asString(),
      wireName,
      streaming,
      style,
      inlineParams,
      requestType,
      responseType,
      hasContextParam,
      /* idempotent */ false,
      isSuspend,
      description,
      meta.tags,
      meta.deprecated,
      fieldMeta,
    )
  }

  private data class MethodMeta(
    val description: String,
    val tags: List<String>,
    val deprecated: Boolean,
  )

  private fun readMethodMeta(fn: KSFunctionDeclaration): MethodMeta {
    fn.getAnnotationsByType(Rpc::class).firstOrNull()?.let {
      return MethodMeta(it.description, it.tags.toList(), it.deprecated)
    }
    fn.getAnnotationsByType(ServerStream::class).firstOrNull()?.let {
      return MethodMeta(it.description, it.tags.toList(), it.deprecated)
    }
    fn.getAnnotationsByType(ClientStream::class).firstOrNull()?.let {
      return MethodMeta(it.description, it.tags.toList(), it.deprecated)
    }
    fn.getAnnotationsByType(BidiStream::class).firstOrNull()?.let {
      return MethodMeta(it.description, it.tags.toList(), it.deprecated)
    }
    return MethodMeta("", emptyList(), false)
  }

  private fun firstParagraph(doc: String?): String {
    if (doc.isNullOrBlank()) return ""
    val sb = StringBuilder()
    for (raw in doc.split("\n")) {
      val line = raw.trim()
      if (line.startsWith("@")) break
      if (line.isEmpty()) {
        if (sb.isNotEmpty()) break
        continue
      }
      if (sb.isNotEmpty()) sb.append(' ')
      sb.append(line)
    }
    return sb.toString()
  }

  private fun streamingKindFor(fn: KSFunctionDeclaration): StreamingKind? {
    if (fn.getAnnotationsByType(Rpc::class).any()) return StreamingKind.UNARY
    if (fn.getAnnotationsByType(ServerStream::class).any()) return StreamingKind.SERVER_STREAM
    if (fn.getAnnotationsByType(ClientStream::class).any()) return StreamingKind.CLIENT_STREAM
    if (fn.getAnnotationsByType(BidiStream::class).any()) return StreamingKind.BIDI_STREAM
    return null
  }

  private fun wireNameFor(fn: KSFunctionDeclaration): String {
    fn.getAnnotationsByType(Rpc::class).firstOrNull()
      ?.let { if (it.name.isNotEmpty()) return it.name }
    fn.getAnnotationsByType(ServerStream::class).firstOrNull()
      ?.let { if (it.name.isNotEmpty()) return it.name }
    fn.getAnnotationsByType(ClientStream::class).firstOrNull()
      ?.let { if (it.name.isNotEmpty()) return it.name }
    fn.getAnnotationsByType(BidiStream::class).firstOrNull()
      ?.let { if (it.name.isNotEmpty()) return it.name }
    return fn.simpleName.asString()
  }

  private fun isCallContext(t: KSType): Boolean =
    t.declaration.qualifiedName?.asString() == "site.aster.interceptors.CallContext"

  private fun looksLikeWireType(tn: TypeName): Boolean {
    if (tn !is ClassName) return false
    val pkg = tn.packageName()
    if (pkg == "java.lang" || pkg == "kotlin") {
      val simple = tn.simpleName()
      return simple !in SCALAR_NAMES
    }
    return true
  }

  private fun ksTypeToTypeName(t: KSType): TypeName? {
    val qn = t.declaration.qualifiedName?.asString() ?: return null
    if (qn == "kotlin.Unit") return null
    return when (qn) {
      "kotlin.String" -> ClassName.get("java.lang", "String")
      "kotlin.Int" -> ClassName.get("java.lang", "Integer")
      "kotlin.Long" -> ClassName.get("java.lang", "Long")
      "kotlin.Short" -> ClassName.get("java.lang", "Short")
      "kotlin.Byte" -> ClassName.get("java.lang", "Byte")
      "kotlin.Float" -> ClassName.get("java.lang", "Float")
      "kotlin.Double" -> ClassName.get("java.lang", "Double")
      "kotlin.Boolean" -> ClassName.get("java.lang", "Boolean")
      "kotlin.Char" -> ClassName.get("java.lang", "Character")
      else -> ClassName.get(t.declaration.packageName.asString(), t.declaration.simpleName.asString())
    }
  }

  private companion object {
    val SCALAR_NAMES = setOf(
      "String", "Integer", "Long", "Short", "Byte", "Float", "Double",
      "Boolean", "Character", "Int", "Char",
    )
  }
}
