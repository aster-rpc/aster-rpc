#!/usr/bin/env python3
"""Linux Java consumer smoke for an exact staged Maven candidate, not publication proof.

Requires site.aster:aster-parent, aster-annotations and aster-runtime POMs,
the annotations/runtime JARs, and the runtime linux-x86_64 classifier JAR.
The native is loaded through that classifier, without a native-path override.
Invoking this helper resolves dependencies and compiles/runs only the consumer.
"""
from __future__ import annotations

import argparse
import hashlib
import json
import os
from pathlib import Path
import platform
import re
import shutil
import subprocess
import tempfile
import zipfile
from xml.sax.saxutils import escape


APP = r'''import java.lang.foreign.*;
import java.nio.file.*;
import java.security.MessageDigest;
import java.util.*;
import java.util.concurrent.*;
import java.util.concurrent.atomic.AtomicInteger;
import com.fasterxml.jackson.databind.ObjectMapper;
import org.apache.fory.Fory;
import org.apache.fory.BaseFory;
import site.aster.annotations.*;
import site.aster.client.*;
import site.aster.codec.*;
import site.aster.config.AsterConfig;
import site.aster.ffi.IrohLibrary;
import site.aster.interceptors.*;
import site.aster.node.NodeAddr;
import site.aster.server.AsterServer;
import site.aster.server.spi.*;

public class Consumer {
  @WireType("interop/EchoRequest") public static final class Request {
    public String message = ""; public int count; public byte[] data = new byte[0];
    public Request() {} Request(String m, int n, byte[] b) { message=m; count=n; data=b; }
  }
  @WireType("interop/EchoReply") public static final class Reply {
    public String message = ""; public int count; public byte[] data = new byte[0]; public String server_lang = "";
    public Reply() {} Reply(Request r, int n) { message=r.message; count=n; data=r.data; server_lang="java"; }
  }
  static final AtomicInteger started = new AtomicInteger(), cancelled = new AtomicInteger();
  static void check(boolean ok, String message) { if (!ok) throw new AssertionError(message); }
  static void register(BaseFory fory) { ForyTags.register(fory, Request.class, "interop/EchoRequest"); ForyTags.register(fory, Reply.class, "interop/EchoReply"); }
  static MethodDescriptor desc(String name, StreamingKind kind) {
    return new MethodDescriptor(name, kind, RequestStyle.EXPLICIT, "interop/EchoRequest", List.of(), "interop/EchoReply", false, false);
  }
  static class Echo implements UnaryDispatcher {
    public MethodDescriptor descriptor() { return desc("echo", StreamingKind.UNARY); }
    public byte[] invoke(Object impl, byte[] payload, Codec codec, CallContext ctx) {
      Request req=(Request)codec.decode(payload, Request.class); return codec.encode(new Reply(req, req.count+1));
    }
  }
  static class Generate implements ServerStreamDispatcher {
    public MethodDescriptor descriptor() { return desc("generate", StreamingKind.SERVER_STREAM); }
    public void invoke(Object impl, byte[] payload, Codec codec, CallContext ctx, ResponseStream out) throws Exception {
      Request req=(Request)codec.decode(payload, Request.class);
      for(int i=0;i<req.count;i++) out.send(codec.encode(new Reply(req,i)));
    }
  }
  static class Hold implements UnaryDispatcher {
    public MethodDescriptor descriptor() { return desc("hold", StreamingKind.UNARY); }
    public byte[] invoke(Object impl, byte[] payload, Codec codec, CallContext ctx) throws Exception {
      started.incrementAndGet();
      for(long end=System.nanoTime()+30_000_000_000L;System.nanoTime()<end;) {
        if(ctx.isCancelled()) { cancelled.incrementAndGet(); throw new RpcError(StatusCode.CANCELLED,"hold: cancelled"); }
        if(ctx.isExpired()) { cancelled.incrementAndGet(); throw new RpcError(StatusCode.DEADLINE_EXCEEDED,"hold: deadline"); }
        Thread.sleep(10);
      }
      throw new AssertionError("hold was not terminated");
    }
  }
  static class Dispatcher implements ServiceDispatcher {
    public ServiceDescriptor descriptor() { return new ServiceDescriptor("InteropEcho",1,Scope.SHARED,Object.class); }
    public Map<String,MethodDispatcher> methods() { return Map.of("echo",new Echo(),"generate",new Generate(),"hold",new Hold()); }
    public Map<String,Class<?>> requestClasses() { return Map.of("echo",Request.class,"generate",Request.class,"hold",Request.class); }
    public Map<String,Class<?>> responseClasses() { return Map.of("echo",Reply.class,"generate",Reply.class,"hold",Reply.class); }
    public void registerTypes(Fory fory) { register(fory); }
  }
  static String hash(Path path) throws Exception { return HexFormat.of().formatHex(MessageDigest.getInstance("SHA-256").digest(Files.readAllBytes(path))); }
  static void waitFor(AtomicInteger counter,int n,String what) throws Exception {
    long end=System.nanoTime()+3_000_000_000L;
    while(counter.get()<n) { if(System.nanoTime()>=end) throw new AssertionError("timed out: "+what); Thread.sleep(10); }
  }
  static Path origin(Class<?> type, String artifact, Path m2, String version) throws Exception {
    Path path=Path.of(type.getProtectionDomain().getCodeSource().getLocation().toURI()).toRealPath();
    check(path.startsWith(m2),"class escaped isolated Maven cache: "+path);
    check(path.getFileName().toString().equals(artifact+"-"+version+".jar"),"class artifact/version mismatch: "+path);
    Properties properties=new Properties();
    try(var in=type.getResourceAsStream("/META-INF/maven/site.aster/"+artifact+"/pom.properties")) {
      check(in!=null,"missing Maven package metadata"); properties.load(in);
    }
    check(version.equals(properties.getProperty("version")),"Maven package metadata version mismatch");
    return path;
  }
  public static void main(String[] args) throws Throwable {
    String version=System.getenv("ASTER_CONSUMER_VERSION"), expectedHash=System.getenv("ASTER_CONSUMER_FFI_SHA256");
    Path m2=Path.of(System.getenv("ASTER_CONSUMER_M2")).toRealPath(), root=Path.of("").toRealPath();
    Path runtime=origin(AsterClient.class,"aster-runtime",m2,version), annotations=origin(WireType.class,"aster-annotations",m2,version);
    var resource=IrohLibrary.class.getResource("/native/linux-x86_64/libaster_transport_ffi.so");
    check(resource!=null && resource.openConnection() instanceof java.net.JarURLConnection,"missing classifier native resource");
    Path nativeJar=Path.of(((java.net.JarURLConnection)resource.openConnection()).getJarFileURL().toURI()).toRealPath();
    check(nativeJar.startsWith(m2),"native resource escaped isolated Maven artifact: "+resource);
    check(nativeJar.getFileName().toString().equals("aster-runtime-"+version+"-linux-x86_64.jar"),"wrong native classifier artifact");
    var library=IrohLibrary.getInstance();
    MemorySegment v=(MemorySegment)library.getHandle("aster_version",FunctionDescriptor.of(ValueLayout.ADDRESS)).invoke();
    String nativeVersion=v.reinterpret(256).getString(0);
    check(version.equals(nativeVersion),"native release version mismatch: "+nativeVersion);
    Set<Path> mapped=new HashSet<>();
    for(String line:Files.readAllLines(Path.of("/proc/self/maps"))) {
      if(line.contains("libaster_transport_ffi.so")) { String[] fields=line.trim().split("\\s+",6); check(fields.length==6,"unreadable native map"); mapped.add(Path.of(fields[5]).toRealPath()); }
    }
    check(mapped.size()==1,"expected one mapped FFI library: "+mapped);
    Path nativePath=mapped.iterator().next();
    check(nativePath.startsWith(root),"mapped native escaped consumer: "+nativePath);
    check(hash(nativePath).equals(expectedHash),"mapped FFI hash mismatch");
    var json=new ObjectMapper();
    System.out.println(json.writeValueAsString(Map.of("phase","java_metadata","version",version,"runtime",runtime.toString(),"annotations",annotations.toString(),"native",nativePath.toString(),"native_sha256",expectedHash,"native_resource",resource.toString(),"abi",library.abiVersion())));
    var config=AsterConfig.builder().relayMode("disabled").bindAddr("127.0.0.1:0").build();
    ForyCodec serverCodec=new ForyCodec(); register(serverCodec.fory());
    AsterServer server=AsterServer.builder().config(config).codec(serverCodec).service(new Object(),new Dispatcher()).build().get(15,TimeUnit.SECONDS);
    AsterClient client=null;
    try {
      ForyCodec clientCodec=new ForyCodec(); register(clientCodec.fory());
      client=AsterClient.builder().config(config).codec(clientCodec).build().get(15,TimeUnit.SECONDS);
      NodeAddr addr=server.node().nodeAddr();
      for(int i=0;addr.directAddresses().isEmpty() && i<100;i++) { Thread.sleep(20); addr=server.node().nodeAddr(); }
      client.connect(addr).get(8,TimeUnit.SECONDS);
      byte[] data=new byte[]{0,1,2,(byte)255};
      Request request=new Request("candidate",41,data);
      Reply reply=client.call(addr,"InteropEcho","echo",request,Reply.class).get(4,TimeUnit.SECONDS);
      check(reply.message.equals(request.message) && reply.count==42 && Arrays.equals(reply.data,data) && reply.server_lang.equals("java"),"unary integrity");
      var rows=client.callServerStream(addr,"InteropEcho","generate",new Request("stream",5,data),Reply.class).get(4,TimeUnit.SECONDS);
      check(rows.size()==5,"stream count");
      for(int i=0;i<rows.size();i++) check(rows.get(i).count==i && rows.get(i).message.equals("stream") && Arrays.equals(rows.get(i).data,data) && rows.get(i).server_lang.equals("java"),"stream integrity");
      long beginning=System.nanoTime();
      try {
        client.call(addr,"InteropEcho","hold",request,Reply.class,CallOptions.DEFAULT.withTimeout(java.time.Duration.ofSeconds(1))).get(4,TimeUnit.SECONDS);
        throw new AssertionError("deadline unexpectedly succeeded");
      } catch(ExecutionException failed) { check(failed.getCause() instanceof RpcError e && e.code()==StatusCode.DEADLINE_EXCEEDED,"wrong deadline error: "+failed); }
      long elapsed=(System.nanoTime()-beginning)/1_000_000;
      check(elapsed>=400 && elapsed<3000,"deadline bound: "+elapsed); waitFor(cancelled,1,"deadline handler cancellation");
      var hold=client.call(addr,"InteropEcho","hold",request,Reply.class,CallOptions.DEFAULT.withTimeout(java.time.Duration.ofSeconds(10)));
      try {
        waitFor(started,2,"caller cancel handler start"); check(hold.cancel(true),"caller cancellation did not own live invocation");
        try { hold.get(1,TimeUnit.SECONDS); throw new AssertionError("cancel unexpectedly succeeded"); } catch(CancellationException expected) {}
        waitFor(cancelled,2,"caller cancellation reaching handler");
      } finally { if(!hold.isDone()) hold.cancel(true); }
    } finally {
      AsterClient owned=client;
      CompletableFuture.runAsync(()->{ if(owned!=null) owned.close(); server.close(); }).get(6,TimeUnit.SECONDS);
      site.aster.otel.Observability.shutdown(java.time.Duration.ofSeconds(2));
    }
    System.out.println(json.writeValueAsString(Map.of("phase","java_result","result","PASS","counts",Map.of("unary",1,"stream",5,"deadline",1,"cancel",1,"shutdown",1))));
  }
}
'''


def sha(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        for block in iter(lambda: stream.read(1024 * 1024), b""):
            digest.update(block)
    return digest.hexdigest()


def run(command: list[str], cwd: Path, env: dict[str, str], log: Path, timeout: int = 240) -> str:
    print("$", " ".join(command), flush=True)
    with log.open("w") as out:
        result = subprocess.run(command, cwd=cwd, env=env, stdout=out, stderr=subprocess.STDOUT, timeout=timeout)
    text = log.read_text(errors="replace")
    print(text[-16000:], end="", flush=True)
    if result.returncode:
        raise SystemExit(f"command failed ({result.returncode}); full log: {log}")
    return text


def main() -> None:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--maven-repo", type=Path, required=True)
    ap.add_argument("--version", required=True)
    ap.add_argument("--ffi-sha256", required=True)
    ap.add_argument("--output", type=Path, required=True)
    ap.add_argument("--checkout-root", type=Path)
    args = ap.parse_args()
    if not re.fullmatch(r"\d+\.\d+\.\d+", args.version) or not re.fullmatch(r"[0-9a-fA-F]{64}", args.ffi_sha256):
        ap.error("exact numeric release version and SHA-256 required")
    if platform.system() != "Linux" or platform.machine().lower() not in ("x86_64", "amd64"):
        ap.error("this smoke is scoped to Linux x86_64")
    repo = args.maven_repo.resolve(strict=True)
    checkout = args.checkout_root.resolve(strict=True) if args.checkout_root else next((p for p in Path(__file__).resolve().parents if (p / ".git").exists()), None)
    if checkout is None:
        ap.error("relocated helper requires --checkout-root")
    output = args.output.resolve()
    if output == checkout or checkout in output.parents or output in checkout.parents:
        ap.error("--output must be outside checkout")
    if output.exists() and any(output.iterdir()):
        ap.error("--output must be fresh and empty")
    candidate = repo / "site/aster"
    artifacts: list[Path] = []
    for artifact in ("aster-parent", "aster-annotations", "aster-runtime"):
        folder = candidate / artifact / args.version
        artifacts.append(folder / f"{artifact}-{args.version}.pom")
        if artifact != "aster-parent":
            artifacts.append(folder / f"{artifact}-{args.version}.jar")
    native_jar = candidate / "aster-runtime" / args.version / f"aster-runtime-{args.version}-linux-x86_64.jar"
    artifacts.append(native_jar)
    for artifact in artifacts:
        if not artifact.is_file():
            ap.error(f"missing staged Maven artifact: {artifact}")
    with zipfile.ZipFile(native_jar) as jar:
        native_hash = hashlib.sha256(jar.read("native/linux-x86_64/libaster_transport_ffi.so")).hexdigest()
    if native_hash != args.ffi_sha256.lower():
        ap.error(f"classifier FFI hash differs from candidate: {native_hash}")
    for tool in ("mvn", "java", "javac"):
        if not shutil.which(tool):
            ap.error(f"{tool} required (JDK 25 or newer)")
    output.mkdir(parents=True)
    project = Path(tempfile.mkdtemp(prefix="consumer-", dir=output))
    feed = output / "feed"
    for artifact in artifacts:
        target = feed / artifact.relative_to(repo)
        target.parent.mkdir(parents=True, exist_ok=True)
        shutil.copy2(artifact, target)
    m2 = output / "m2"
    native_tmp = project / "native-temp"
    native_tmp.mkdir()
    (project / "Consumer.java").write_text(APP)
    (project / "pom.xml").write_text(f'''<project xmlns="http://maven.apache.org/POM/4.0.0"><modelVersion>4.0.0</modelVersion>
<groupId>assurance</groupId><artifactId>clean-consumer</artifactId><version>1</version>
<repositories><repository><id>candidate</id><url>{escape(feed.as_uri())}</url></repository></repositories>
<dependencies><dependency><groupId>site.aster</groupId><artifactId>aster-runtime</artifactId><version>{args.version}</version></dependency>
<dependency><groupId>site.aster</groupId><artifactId>aster-runtime</artifactId><version>{args.version}</version><classifier>linux-x86_64</classifier></dependency></dependencies></project>\n''')
    settings = output / "settings.xml"
    settings.write_text('<settings xmlns="http://maven.apache.org/SETTINGS/1.0.0"><interactiveMode>false</interactiveMode></settings>\n')
    env = os.environ.copy()
    for key in list(env):
        if key.startswith(("ASTER_", "OTEL_")):
            env.pop(key, None)
    for key in ("CLASSPATH", "JAVA_TOOL_OPTIONS", "_JAVA_OPTIONS", "JDK_JAVA_OPTIONS", "MAVEN_OPTS", "MAVEN_ARGS", "IROH_LIB_PATH", "LD_PRELOAD", "LD_LIBRARY_PATH", "ASTER_STORAGE_PATH", "ASTER_SECRET_KEY_FILE", "ASTER_IDENTITY_FILE"):
        env.pop(key, None)
    env.update(ASTER_CONSUMER_VERSION=args.version, ASTER_CONSUMER_FFI_SHA256=native_hash, ASTER_CONSUMER_M2=str(m2))
    metadata = {"scope": "staged_maven_artifact_consumer", "public_registry_proof": False, "version": args.version,
                "ffi_sha256": native_hash, "candidate_artifacts": {str(p.relative_to(repo)): sha(p) for p in artifacts}, "project": str(project)}
    print(json.dumps(metadata, sort_keys=True), flush=True)
    run(["mvn", "-B", "-s", str(settings), "-gs", str(settings), f"-Dmaven.repo.local={m2}",
         "org.apache.maven.plugins:maven-dependency-plugin:3.6.1:build-classpath", "-Dmdep.outputFile=classpath.txt"], project, env, output / "resolve.log")
    classpath = (project / "classpath.txt").read_text().strip()
    for entry in classpath.split(os.pathsep):
        path = Path(entry).resolve(strict=True)
        if m2 not in path.parents:
            raise SystemExit(f"classpath escaped isolated cache: {path}")
    for artifact in artifacts:
        resolved = m2 / artifact.relative_to(repo)
        if not resolved.is_file() or sha(resolved) != sha(artifact):
            raise SystemExit(f"Maven did not resolve exact staged candidate: {resolved}")
    run(["javac", "-proc:none", "--release", "25", "-cp", classpath, "Consumer.java"], project, env, output / "compile.log")
    text = run(["java", "--enable-native-access=ALL-UNNAMED", f"-Djava.io.tmpdir={native_tmp}", "-cp", str(project) + os.pathsep + classpath, "Consumer"], project, env, output / "consumer.log", 60)
    if not any(json.loads(line).get("phase") == "java_result" and json.loads(line).get("result") == "PASS" for line in text.splitlines() if line.startswith("{")):
        raise SystemExit("consumer did not emit its success receipt")
    metadata["result"] = "PASS"
    (output / "result.json").write_text(json.dumps(metadata, indent=2) + "\n")
    print(f"JAVA_CLEAN_CONSUMER_PASS report_dir={output}", flush=True)


if __name__ == "__main__":
    main()
