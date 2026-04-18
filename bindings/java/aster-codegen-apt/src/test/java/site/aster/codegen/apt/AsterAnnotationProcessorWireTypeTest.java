package site.aster.codegen.apt;

import static org.junit.jupiter.api.Assertions.assertFalse;
import static org.junit.jupiter.api.Assertions.assertNotNull;
import static org.junit.jupiter.api.Assertions.assertTrue;

import java.io.IOException;
import java.io.StringWriter;
import java.nio.charset.StandardCharsets;
import java.nio.file.Files;
import java.nio.file.Path;
import java.util.List;
import java.util.Locale;
import java.util.stream.Stream;
import javax.tools.Diagnostic;
import javax.tools.DiagnosticCollector;
import javax.tools.JavaCompiler;
import javax.tools.JavaFileObject;
import javax.tools.StandardJavaFileManager;
import javax.tools.StandardLocation;
import javax.tools.ToolProvider;
import org.junit.jupiter.api.Test;
import org.junit.jupiter.api.io.TempDir;

/**
 * End-to-end verification that {@code @WireType("ns/Name")} on request / response / inline-param
 * types flows through the APT processor and overrides the generated Fory tag. Cross-language
 * contract_id parity depends on this tag matching Python's {@code @wire_type(...)} value.
 */
final class AsterAnnotationProcessorWireTypeTest {

  @Test
  void explicitRequestAndResponseUseWireTypeTag(@TempDir Path workspace) throws Exception {
    writeSource(
        workspace,
        "demo/BillingService.java",
        """
        package demo;

        import site.aster.annotations.Rpc;
        import site.aster.annotations.Service;
        import site.aster.annotations.WireType;

        @WireType("billing/Invoice")
        record Invoice(String customer, long amountCents) {}

        @WireType("billing/PaymentConfirmation")
        record PaymentConfirmation(String txId, boolean paid) {}

        @Service(name = "Billing", version = 1)
        class BillingService {
            @Rpc public PaymentConfirmation pay(Invoice inv) {
                return new PaymentConfirmation("tx", true);
            }
        }
        """);

    String dispatcher = assertCompiles(workspace, "demo/BillingService$AsterDispatcher.java");

    assertTrue(
        dispatcher.contains("safeRegister(fory, Invoice.class, \"billing/Invoice\")"), dispatcher);
    assertTrue(
        dispatcher.contains(
            "safeRegister(fory, PaymentConfirmation.class, \"billing/PaymentConfirmation\")"),
        dispatcher);
    // The derived-from-Java-package tag must not leak.
    assertFalse(dispatcher.contains("\"demo/Invoice\""), dispatcher);
    assertFalse(dispatcher.contains("\"demo/PaymentConfirmation\""), dispatcher);
  }

  @Test
  void inlineParamTypeUsesWireTypeTag(@TempDir Path workspace) throws Exception {
    writeSource(
        workspace,
        "demo/StampService.java",
        """
        package demo;

        import site.aster.annotations.Rpc;
        import site.aster.annotations.Service;
        import site.aster.annotations.WireType;

        @WireType("billing/Invoice")
        record Invoice(String customer, long amountCents) {}

        record Ack(boolean ok) {}

        @Service(name = "Stamp", version = 1)
        class StampService {
            @Rpc public Ack stamp(Invoice invoice) { return new Ack(true); }
        }
        """);

    String dispatcher = assertCompiles(workspace, "demo/StampService$AsterDispatcher.java");

    // Inline style, so invoice goes through as an inline param and gets registered separately.
    assertTrue(
        dispatcher.contains("safeRegister(fory, Invoice.class, \"billing/Invoice\")"), dispatcher);
    // Ack has no @WireType so it falls back to Java package.
    assertTrue(dispatcher.contains("safeRegister(fory, Ack.class, \"demo/Ack\")"), dispatcher);
  }

  @Test
  void typeWithoutWireTypeUsesDerivedFallback(@TempDir Path workspace) throws Exception {
    writeSource(
        workspace,
        "demo/PlainService.java",
        """
        package demo;

        import site.aster.annotations.Rpc;
        import site.aster.annotations.Service;

        record PlainRequest(String x) {}
        record PlainResponse(String y) {}

        @Service(name = "Plain", version = 1)
        class PlainService {
            @Rpc public PlainResponse run(PlainRequest r) {
                return new PlainResponse("ok");
            }
        }
        """);

    String dispatcher = assertCompiles(workspace, "demo/PlainService$AsterDispatcher.java");

    assertTrue(
        dispatcher.contains("safeRegister(fory, PlainRequest.class, \"demo/PlainRequest\")"),
        dispatcher);
    assertTrue(
        dispatcher.contains("safeRegister(fory, PlainResponse.class, \"demo/PlainResponse\")"),
        dispatcher);
  }

  // ────────────────────────────────────────────────────────────────────────
  // Harness — same shape as AsterAnnotationProcessorMetadataTest
  // ────────────────────────────────────────────────────────────────────────

  private static String assertCompiles(Path workspace, String generatedFile) throws IOException {
    CompileResult result = compile(workspace);
    assertTrue(result.success(), "compile failed:\n" + result.diagnostics());
    String file = result.read(generatedFile);
    assertNotNull(file, "expected " + generatedFile + " to be generated");
    return file;
  }

  private static void writeSource(Path workspace, String relative, String body) throws IOException {
    Path p = workspace.resolve("sources").resolve(relative);
    Files.createDirectories(p.getParent());
    Files.writeString(p, body, StandardCharsets.UTF_8);
  }

  private static CompileResult compile(Path workspace) throws IOException {
    Path sources = workspace.resolve("sources");
    Path generated = workspace.resolve("generated");
    Path classOutput = workspace.resolve("classes");
    Files.createDirectories(generated);
    Files.createDirectories(classOutput);

    JavaCompiler compiler = ToolProvider.getSystemJavaCompiler();
    DiagnosticCollector<JavaFileObject> diagnostics = new DiagnosticCollector<>();
    StandardJavaFileManager fm =
        compiler.getStandardFileManager(diagnostics, Locale.ROOT, StandardCharsets.UTF_8);

    fm.setLocation(StandardLocation.SOURCE_OUTPUT, List.of(generated.toFile()));
    fm.setLocation(StandardLocation.CLASS_OUTPUT, List.of(classOutput.toFile()));

    Iterable<? extends JavaFileObject> units;
    try (Stream<Path> s = Files.walk(sources)) {
      var files = s.filter(p -> p.toString().endsWith(".java")).map(Path::toFile).toList();
      units = fm.getJavaFileObjectsFromFiles(files);
    }

    StringWriter out = new StringWriter();
    List<String> options =
        List.of(
            "-proc:only",
            "-processor",
            AsterAnnotationProcessor.class.getName(),
            "-s",
            generated.toString());

    boolean success = compiler.getTask(out, fm, diagnostics, options, null, units).call();
    fm.close();
    return new CompileResult(success, diagnostics, generated, classOutput);
  }

  record CompileResult(
      boolean success,
      DiagnosticCollector<JavaFileObject> collector,
      Path generatedSources,
      Path classOutput) {

    String read(String relative) throws IOException {
      Path a = generatedSources.resolve(relative);
      if (Files.exists(a)) {
        return Files.readString(a);
      }
      Path b = classOutput.resolve(relative);
      if (Files.exists(b)) {
        return Files.readString(b);
      }
      return null;
    }

    String diagnostics() {
      StringBuilder sb = new StringBuilder();
      for (Diagnostic<? extends JavaFileObject> d : collector.getDiagnostics()) {
        sb.append(d).append('\n');
      }
      return sb.toString();
    }
  }
}
