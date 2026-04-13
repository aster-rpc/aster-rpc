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
 * End-to-end test for {@link AsterAnnotationProcessor}. Writes a synthetic {@code @Service} source
 * to a temp directory, runs javac with the processor attached (process-only mode), and reads the
 * generated sources back from the same temp directory.
 *
 * <p>Temp-dir + real StandardJavaFileManager is more robust than an in-memory FileManager because
 * it exercises the same code path javac will use at production time.
 */
final class AsterAnnotationProcessorTest {

  @Test
  void unaryExplicitServiceGeneratesDispatcher(@TempDir Path workspace) throws Exception {
    writeSource(
        workspace,
        "demo/DemoService.java",
        """
        package demo;

        import site.aster.annotations.Rpc;
        import site.aster.annotations.Scope;
        import site.aster.annotations.Service;

        class StatusRequest { public String agentId; }
        record StatusResponse(String agentId, String state) {}

        @Service(name = "Demo", version = 1, scoped = Scope.SHARED)
        class DemoService {
            @Rpc
            public StatusResponse getStatus(StatusRequest req) {
                return new StatusResponse(req.agentId, "running");
            }
        }
        """);

    CompileResult result = compile(workspace);
    assertTrue(result.success(), "compile failed:\n" + result.diagnostics());

    String dispatcher = result.read("demo/DemoService$AsterDispatcher.java");
    assertNotNull(dispatcher, "dispatcher source not generated");
    assertTrue(dispatcher.contains("implements ServiceDispatcher"));
    assertTrue(dispatcher.contains("GetStatus$Dispatcher"));
    assertTrue(dispatcher.contains("((DemoService) impl).getStatus(request)"));

    String svcFile = result.read("META-INF/services/site.aster.server.spi.ServiceDispatcher");
    assertNotNull(svcFile, "META-INF/services file not generated");
    assertTrue(svcFile.contains("demo.DemoService$AsterDispatcher"));
  }

  @Test
  void unaryInlineWithCtxGeneratesUnpackedDispatch(@TempDir Path workspace) throws Exception {
    writeSource(
        workspace,
        "demo/AgentService.java",
        """
        package demo;

        import site.aster.annotations.Rpc;
        import site.aster.annotations.Service;
        import site.aster.interceptors.CallContext;

        record Assignment(String taskId, String command) {}

        @Service(name = "Agent", version = 1)
        class AgentService {
            @Rpc
            public Assignment heartbeat(String agentId, CallContext ctx) {
                return new Assignment("idle", "sleep 60");
            }
        }
        """);

    CompileResult result = compile(workspace);
    assertTrue(result.success(), "compile failed:\n" + result.diagnostics());

    String dispatcher = result.read("demo/AgentService$AsterDispatcher.java");
    assertNotNull(dispatcher);
    assertTrue(dispatcher.contains("AgentService_HeartbeatRequest"));
    assertTrue(dispatcher.contains("((AgentService) impl).heartbeat(inline.agentId(), ctx)"));

    String record = result.read("demo/AgentService_HeartbeatRequest.java");
    assertNotNull(record, "inline request record not generated");
    assertTrue(record.contains("record AgentService_HeartbeatRequest"));
    assertTrue(record.contains("String agentId"));
  }

  @Test
  void sessionScopeProducesScopeSession(@TempDir Path workspace) throws Exception {
    writeSource(
        workspace,
        "demo/AgentSessionService.java",
        """
        package demo;

        import site.aster.annotations.Rpc;
        import site.aster.annotations.Scope;
        import site.aster.annotations.Service;

        record Heartbeat(String agentId) {}
        record Ack() {}

        @Service(name = "AgentSession", version = 1, scoped = Scope.SESSION)
        class AgentSessionService {
            @Rpc
            public Ack register(Heartbeat hb) { return new Ack(); }
        }
        """);

    CompileResult result = compile(workspace);
    assertTrue(result.success(), "compile failed:\n" + result.diagnostics());

    String dispatcher = result.read("demo/AgentSessionService$AsterDispatcher.java");
    assertNotNull(dispatcher);
    assertTrue(dispatcher.contains("Scope.SESSION"));
  }

  @Test
  void serverStreamStubContainsStreamingInterface(@TempDir Path workspace) throws Exception {
    writeSource(
        workspace,
        "demo/LogsService.java",
        """
        package demo;

        import site.aster.annotations.ServerStream;
        import site.aster.annotations.Service;

        record TailRequest(String agentId, String level) {}
        record LogEntry(String agentId, String level, String message) {}

        @Service(name = "Logs", version = 1)
        class LogsService {
            @ServerStream
            public void tailLogs(TailRequest req) { }
        }
        """);

    CompileResult result = compile(workspace);
    assertTrue(result.success(), "compile failed:\n" + result.diagnostics());

    String dispatcher = result.read("demo/LogsService$AsterDispatcher.java");
    assertNotNull(dispatcher);
    assertTrue(dispatcher.contains("implements ServerStreamDispatcher"));
    assertTrue(dispatcher.contains("throw new UnsupportedOperationException"));
  }

  // ────────────────────────────────────────────────────────────────────────
  // Harness
  // ────────────────────────────────────────────────────────────────────────

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
