package site.aster.codegen.apt;

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
 * Verifies that rich-metadata (descriptions, tags, deprecated flag, field metadata) flows
 * end-to-end from annotated source → APT model → generated dispatcher constants.
 *
 * <p>Uses the same temp-dir + javac harness as {@link AsterAnnotationProcessorTest} so regressions
 * here are real compiler failures, not mocked model walks.
 */
final class AsterAnnotationProcessorMetadataTest {

  @Test
  void serviceAnnotationDescriptionAndTagsPropagate(@TempDir Path workspace) throws Exception {
    writeSource(
        workspace,
        "demo/Meta1Service.java",
        """
        package demo;

        import site.aster.annotations.Rpc;
        import site.aster.annotations.Service;

        record PingResponse() {}

        @Service(
            name = "Meta1",
            version = 1,
            description = "Explicit service description.",
            tags = {"readonly", "experimental"})
        class Meta1Service {
            @Rpc public PingResponse ping() { return new PingResponse(); }
        }
        """);

    String dispatcher = assertCompiles(workspace, "demo/Meta1Service$AsterDispatcher.java");
    assertTrue(dispatcher.contains("DESCRIPTION = \"Explicit service description.\""), dispatcher);
    assertTrue(dispatcher.contains("List.of(\"readonly\", \"experimental\")"), dispatcher);
    assertTrue(dispatcher.contains("public String description()"), dispatcher);
    assertTrue(dispatcher.contains("public List<String> tags()"), dispatcher);
  }

  @Test
  void javadocFallsBackForServiceDescription(@TempDir Path workspace) throws Exception {
    writeSource(
        workspace,
        "demo/Meta2Service.java",
        """
        package demo;

        import site.aster.annotations.Rpc;
        import site.aster.annotations.Service;

        record PingResponse() {}

        /**
         * Javadoc-derived description.
         *
         * Additional text that must not leak.
         */
        @Service(name = "Meta2", version = 1)
        class Meta2Service {
            @Rpc public PingResponse ping() { return new PingResponse(); }
        }
        """);

    String dispatcher = assertCompiles(workspace, "demo/Meta2Service$AsterDispatcher.java");
    assertTrue(dispatcher.contains("DESCRIPTION = \"Javadoc-derived description.\""), dispatcher);
  }

  @Test
  void methodAnnotationDescriptionAndTagsLandInMap(@TempDir Path workspace) throws Exception {
    writeSource(
        workspace,
        "demo/Meta3Service.java",
        """
        package demo;

        import site.aster.annotations.Rpc;
        import site.aster.annotations.Service;

        record StatusRequest(String agentId) {}
        record StatusResponse(String state) {}

        @Service(name = "Meta3", version = 1)
        class Meta3Service {
            @Rpc(
                description = "Fetch agent status.",
                tags = {"readonly"},
                deprecated = true)
            public StatusResponse getStatus(StatusRequest req) {
                return new StatusResponse("ok");
            }
        }
        """);

    String dispatcher = assertCompiles(workspace, "demo/Meta3Service$AsterDispatcher.java");
    assertTrue(dispatcher.contains("Map.entry(\"getStatus\""), dispatcher);
    assertTrue(dispatcher.contains("\"Fetch agent status.\""), dispatcher);
    assertTrue(dispatcher.contains("List.of(\"readonly\")"), dispatcher);
    assertTrue(
        dispatcher.contains(
            "new MethodMetadata(\"Fetch agent status.\", List.of(\"readonly\"), true,"),
        dispatcher);
  }

  @Test
  void recordComponentDescriptionsPropagate(@TempDir Path workspace) throws Exception {
    writeSource(
        workspace,
        "demo/Meta4Service.java",
        """
        package demo;

        import site.aster.annotations.Description;
        import site.aster.annotations.Rpc;
        import site.aster.annotations.Service;

        record StatusRequest(
            @Description(value = "Unique agent id.", tags = {"pii"}) String agentId,
            @Description("BCP 47 locale.") String locale) {}

        record StatusResponse(String state) {}

        @Service(name = "Meta4", version = 1)
        class Meta4Service {
            @Rpc public StatusResponse getStatus(StatusRequest req) {
                return new StatusResponse("ok");
            }
        }
        """);

    String dispatcher = assertCompiles(workspace, "demo/Meta4Service$AsterDispatcher.java");
    assertTrue(dispatcher.contains("Map.entry(\"agentId\", new FieldMetadata"), dispatcher);
    assertTrue(dispatcher.contains("\"Unique agent id.\""), dispatcher);
    assertTrue(dispatcher.contains("List.of(\"pii\")"), dispatcher);
    assertTrue(dispatcher.contains("Map.entry(\"locale\", new FieldMetadata"), dispatcher);
    assertTrue(dispatcher.contains("\"BCP 47 locale.\""), dispatcher);
  }

  @Test
  void inlineParamDescriptionsBecomeFieldMetadata(@TempDir Path workspace) throws Exception {
    writeSource(
        workspace,
        "demo/Meta5Service.java",
        """
        package demo;

        import site.aster.annotations.Description;
        import site.aster.annotations.Rpc;
        import site.aster.annotations.Service;

        record RegisterResult(String ok) {}

        @Service(name = "Meta5", version = 1)
        class Meta5Service {
            @Rpc
            public RegisterResult register(
                @Description("Agent identifier.") String agentId,
                @Description(value = "API token.", tags = {"secret"}) String apiKey) {
                return new RegisterResult("ok");
            }
        }
        """);

    String dispatcher = assertCompiles(workspace, "demo/Meta5Service$AsterDispatcher.java");
    assertTrue(dispatcher.contains("Map.entry(\"agentId\", new FieldMetadata"), dispatcher);
    assertTrue(dispatcher.contains("\"Agent identifier.\""), dispatcher);
    assertTrue(dispatcher.contains("Map.entry(\"apiKey\", new FieldMetadata"), dispatcher);
    assertTrue(dispatcher.contains("List.of(\"secret\")"), dispatcher);
  }

  @Test
  void methodJavadocFallsBackWhenAnnotationBlank(@TempDir Path workspace) throws Exception {
    writeSource(
        workspace,
        "demo/Meta6Service.java",
        """
        package demo;

        import site.aster.annotations.Rpc;
        import site.aster.annotations.Service;

        record StatusRequest(String agentId) {}
        record StatusResponse(String state) {}

        @Service(name = "Meta6", version = 1)
        class Meta6Service {
            /**
             * Gets the agent status from persistent store.
             *
             * Side note that must not leak.
             *
             * @param req request
             * @return status
             */
            @Rpc(tags = {"readonly"})
            public StatusResponse getStatus(StatusRequest req) {
                return new StatusResponse("ok");
            }
        }
        """);

    String dispatcher = assertCompiles(workspace, "demo/Meta6Service$AsterDispatcher.java");
    assertTrue(dispatcher.contains("\"Gets the agent status from persistent store.\""), dispatcher);
    // @param line must NOT leak into the description
    assertTrue(!dispatcher.contains("@param req"), "description must stop at first @ tag");
  }

  // ────────────────────────────────────────────────────────────────────────
  // Harness (copied from AsterAnnotationProcessorTest — keep them independent so
  // moving the other file won't silently break these).
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
