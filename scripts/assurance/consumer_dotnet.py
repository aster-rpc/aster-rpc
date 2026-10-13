#!/usr/bin/env python3
"""Run a clean .NET consumer against supplied local Aster and Linux FFI artifacts.

The consumer project is generated under a fresh temporary directory outside the
checkout. Aster restores only from the supplied local NuGet package; its exact
Apache.Fory fork restores from Forgejo, and other dependencies from nuget.org.
The separately supplied FFI library is
copied next to the application so .NET's native loader resolves the candidate.
This script builds/runs the consumer when invoked; it does not build Aster.
"""
from __future__ import annotations

import argparse
import hashlib
import json
import os
from pathlib import Path
import platform
import shutil
import subprocess
import tempfile
import re
import zipfile
import xml.etree.ElementTree as ET


PROGRAM = r'''using System.Diagnostics;
using System.Reflection;
using System.Security.Cryptography;
using System.Text.Json;
using Aster;
using Aster.Interceptors;
using Aster.Rpc;
using Apache.Fory;

static void Check(bool ok, string message) { if (!ok) throw new Exception(message); }
static string Sha256(string path) => Convert.ToHexString(SHA256.HashData(File.ReadAllBytes(path))).ToLowerInvariant();
static async Task WaitFor(Func<bool> predicate, string what) {
    var until = DateTime.UtcNow + TimeSpan.FromSeconds(4);
    while (!predicate()) { if (DateTime.UtcNow >= until) throw new TimeoutException($"Timed out waiting for {what}"); await Task.Delay(10); }
}

string expected = Environment.GetEnvironmentVariable("ASTER_CONSUMER_VERSION") ?? throw new Exception("version missing");
string expectedNativeHash = Environment.GetEnvironmentVariable("ASTER_CONSUMER_FFI_SHA256") ?? throw new Exception("native hash missing");
string expectedNativePath = Path.GetFullPath(Environment.GetEnvironmentVariable("ASTER_CONSUMER_FFI_PATH") ?? throw new Exception("native path missing"));
string appRoot = Path.GetFullPath(AppContext.BaseDirectory);
var asm = typeof(AsterServer).Assembly;
string assemblyPath = Path.GetFullPath(asm.Location);
Check(assemblyPath.StartsWith(appRoot, StringComparison.Ordinal), $"Aster assembly escaped clean consumer output: {assemblyPath}");
string? infoVersion = asm.GetCustomAttribute<AssemblyInformationalVersionAttribute>()?.InformationalVersion;
Check(infoVersion is not null && (infoVersion == expected || infoVersion.StartsWith(expected + "+", StringComparison.Ordinal)),
    $"Aster assembly informational version {infoVersion ?? "<missing>"} does not match package {expected}");

int cancelled = 0;
var service = ServiceDefinition.Create("AssuranceCleanConsumer", 1)
    .Unary<Request, Reply>("echo", (req, _) => ValueTask.FromResult(new Reply { Value = "echo:" + req.Value, Count = req.Count }))
    .ServerStream<Request, Reply>("sequence", Sequence)
    .Unary<Request, Reply>("hold", async (_, ctx) => {
        try { await Task.Delay(TimeSpan.FromSeconds(30), ctx.CancellationToken); }
        catch (OperationCanceledException) { Interlocked.Increment(ref cancelled); throw; }
        return new Reply { Value = "unexpected" };
    }).Build();
var config = new AsterConfig { RelayMode = "disabled", BindAddr = "127.0.0.1:0" };
int unary = 0, streamed = 0, deadline = 0, cancel = 0, shutdown = 0;
try {
    await using (var server = await AsterServer.StartAsync(new AsterServerOptions {
        Config = config, Services = { service }
    })) {
        await using (var client = await AsterClient.StartAsync(config)) {
            client.RegisterTypes(service);
            // Node startup forces the P/Invoke library load. Verify only after this point.
            Check(Sha256(expectedNativePath) == expectedNativeHash, "copied FFI hash differs from candidate");
            var mappings = File.ReadAllLines("/proc/self/maps");
            string? nativeMap = mappings.FirstOrDefault(line => line.Contains(expectedNativePath, StringComparison.Ordinal));
            Check(nativeMap is not null, $"candidate FFI library is not present in /proc/self/maps: {expectedNativePath}");
            Console.WriteLine(JsonSerializer.Serialize(new { phase="dotnet_metadata", version=expected,
                assembly=assemblyPath, assemblyVersion=asm.GetName().Version?.ToString(), informationalVersion=infoVersion,
                nativePath=expectedNativePath, nativeSha256=expectedNativeHash,
                runtime=Environment.Version.ToString(), os=Environment.OSVersion.ToString() }));
            var conn = await client.ConnectAsync(await server.TicketAsync()).WaitAsync(TimeSpan.FromSeconds(12));
            var reply = await conn.UnaryAsync<Request, Reply>("AssuranceCleanConsumer", "echo", new Request { Value="candidate", Count=17 }).WaitAsync(TimeSpan.FromSeconds(8));
            Check(reply.Value == "echo:candidate" && reply.Count == 17, "unary response mismatch"); unary++;
            var rows = new List<Reply>();
            using (var streamTimeout = new CancellationTokenSource(TimeSpan.FromSeconds(8)))
                await foreach (var row in conn.ServerStreamAsync<Request, Reply>("AssuranceCleanConsumer", "sequence", new Request { Value="stream", Count=5 }, cancellationToken: streamTimeout.Token)) rows.Add(row);
            Check(rows.Count == 5 && rows.Select(row => row.Count).SequenceEqual(Enumerable.Range(0, 5)), "server stream mismatch"); streamed = rows.Count;
            var sw = Stopwatch.StartNew();
            try {
                await conn.UnaryAsync<Request, Reply>("AssuranceCleanConsumer", "hold", new Request { Value="deadline" }, new CallOptions { Deadline=TimeSpan.FromSeconds(1) }).WaitAsync(TimeSpan.FromSeconds(5));
                throw new Exception("deadline call unexpectedly succeeded");
            } catch (RpcError e) when (e.Code == StatusCode.DeadlineExceeded) { }
            sw.Stop(); Check(sw.Elapsed >= TimeSpan.FromMilliseconds(400) && sw.Elapsed < TimeSpan.FromSeconds(4), $"deadline elapsed outside expected bound: {sw.Elapsed}");
            await WaitFor(() => Volatile.Read(ref cancelled) >= 1, "deadline cancellation in handler"); deadline++;
            using (var cts = new CancellationTokenSource(TimeSpan.FromMilliseconds(250))) {
                try {
                    await conn.UnaryAsync<Request, Reply>("AssuranceCleanConsumer", "hold", new Request { Value="cancel" }, null, cts.Token).WaitAsync(TimeSpan.FromSeconds(4));
                    throw new Exception("cancelled call unexpectedly succeeded");
                } catch (OperationCanceledException) when (cts.IsCancellationRequested) { }
            }
            await WaitFor(() => Volatile.Read(ref cancelled) >= 2, "caller cancellation in handler"); cancel++;
        }
    }
    shutdown++;
} catch { throw; }
Check(unary == 1 && streamed == 5 && deadline == 1 && cancel == 1 && shutdown == 1, "one or more consumer assertions did not complete");
Console.WriteLine(JsonSerializer.Serialize(new { phase="dotnet_result", result="PASS", counts=new { unary, serverStream=streamed, deadline, cancel, shutdown } }));

static async IAsyncEnumerable<Reply> Sequence(Request request, ServerCallContext context) {
    for (int i=0; i<request.Count; i++) { context.CancellationToken.ThrowIfCancellationRequested(); yield return new Reply { Value=request.Value, Count=i }; await Task.Yield(); }
}

[ForyStruct, WireType("assurance.clean/Request")]
public sealed class Request { public string Value { get; set; } = ""; public int Count { get; set; } }
[ForyStruct, WireType("assurance.clean/Reply")]
public sealed class Reply { public string Value { get; set; } = ""; public int Count { get; set; } }
'''


def sha256(path: Path) -> str:
    h = hashlib.sha256()
    with path.open("rb") as stream:
        for block in iter(lambda: stream.read(1024 * 1024), b""):
            h.update(block)
    return h.hexdigest()


def run(cmd: list[str], *, cwd: Path, env: dict[str, str], log: Path) -> None:
    with log.open("w", encoding="utf-8") as out:
        print("$", " ".join(cmd), flush=True)
        result = subprocess.run(cmd, cwd=cwd, env=env, stdout=out, stderr=subprocess.STDOUT)
    print(log.read_text(errors="replace")[-16000:], end="", flush=True)
    if result.returncode:
        raise SystemExit(f"command failed ({result.returncode}); full log: {log}")


def main() -> None:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--nupkg", type=Path, required=True, help="candidate Aster .nupkg")
    ap.add_argument("--nupkg-sha256", required=True)
    ap.add_argument("--ffi", type=Path, required=True, help="candidate Linux x86_64 libaster_transport_ffi.so")
    ap.add_argument("--ffi-sha256", required=True)
    ap.add_argument("--version", required=True, help="exact stamped NuGet version")
    ap.add_argument("--output", type=Path, required=True, help="directory for logs and result metadata")
    ap.add_argument("--checkout-root", type=Path, help="source checkout to exclude; required when this harness is copied outside the checkout")
    a = ap.parse_args()
    if not re.fullmatch(r"[0-9]+\.[0-9]+\.[0-9]+", a.version):
        ap.error("--version must be an exact numeric major.minor.patch")
    nupkg, ffi = a.nupkg.resolve(strict=True), a.ffi.resolve(strict=True)
    for label, path, expected in (("nupkg", nupkg, a.nupkg_sha256), ("ffi", ffi, a.ffi_sha256)):
        actual = sha256(path)
        if actual.lower() != expected.lower():
            raise SystemExit(f"SHA-256 mismatch for {label}: {actual} != {expected}")
    if nupkg.suffix.lower() != ".nupkg" or ffi.name != "libaster_transport_ffi.so":
        raise SystemExit("expected .nupkg and libaster_transport_ffi.so inputs")
    try:
        with zipfile.ZipFile(nupkg) as archive:
            nuspec = next(name for name in archive.namelist() if name.lower().endswith(".nuspec"))
            metadata = ET.fromstring(archive.read(nuspec))
        package = metadata.find(".//{*}metadata")
        package_id = package.findtext("{*}id") if package is not None else None
        package_version = package.findtext("{*}version") if package is not None else None
    except (zipfile.BadZipFile, StopIteration, ET.ParseError) as exc:
        raise SystemExit(f"cannot read nupkg metadata: {exc}") from exc
    if package_id is None or package_id.lower() != "aster" or package_version != a.version:
        raise SystemExit(f"nupkg metadata mismatch: id={package_id!r}, version={package_version!r}; expected Aster {a.version}")
    if platform.system() != "Linux" or platform.machine().lower() not in ("x86_64", "amd64"):
        raise SystemExit("this consumer smoke is scoped to Linux x86_64")
    dotnet = shutil.which("dotnet")
    if not dotnet:
        raise SystemExit("dotnet is required")

    if a.checkout_root is not None:
        repo = a.checkout_root.resolve(strict=True)
    else:
        repo = next((p for p in Path(__file__).resolve().parents if (p / ".git").exists()), None)
        if repo is None:
            ap.error("relocated harness requires --checkout-root")
    output = a.output.resolve()
    if output == repo or repo in output.parents or output in repo.parents:
        raise SystemExit(f"consumer output must be outside checkout: {output}")
    if output.exists() and any(output.iterdir()):
        raise SystemExit(f"consumer output must be fresh and empty: {output}")
    output.mkdir(parents=True, exist_ok=True)
    project = output / "consumer"
    project.mkdir(exist_ok=True)
    feed = output / "feed"
    feed.mkdir(exist_ok=True)
    shutil.copy2(nupkg, feed / nupkg.name)
    native = project / "libaster_transport_ffi.so"
    shutil.copy2(ffi, native)
    (project / "Program.cs").write_text(PROGRAM, encoding="utf-8")
    (project / "Consumer.csproj").write_text(f'''<Project Sdk="Microsoft.NET.Sdk">
  <PropertyGroup><OutputType>Exe</OutputType><TargetFramework>net9.0</TargetFramework><ImplicitUsings>enable</ImplicitUsings><Nullable>enable</Nullable><TreatWarningsAsErrors>true</TreatWarningsAsErrors></PropertyGroup>
  <ItemGroup><PackageReference Include="Aster" Version="{a.version}" /></ItemGroup>
  <ItemGroup><None Include="libaster_transport_ffi.so" CopyToOutputDirectory="PreserveNewest" /></ItemGroup>
</Project>
''', encoding="utf-8")
    (project / "NuGet.Config").write_text(f'''<?xml version="1.0" encoding="utf-8"?>
<configuration>
  <packageSources>
    <clear />
    <add key="candidate" value="{feed}" />
    <add key="forgejo" value="https://forge.emrul.dev/api/packages/Aster/nuget/index.json" />
    <add key="nuget.org" value="https://api.nuget.org/v3/index.json" />
  </packageSources>
  <packageSourceMapping>
    <clear />
    <packageSource key="candidate"><package pattern="Aster" /></packageSource>
    <packageSource key="forgejo"><package pattern="Apache.Fory" /></packageSource>
    <packageSource key="nuget.org"><package pattern="*" /></packageSource>
  </packageSourceMapping>
</configuration>
''', encoding="utf-8")

    env = os.environ.copy()
    for key in ("DOTNET_ADDITIONAL_DEPS", "DOTNET_SHARED_STORE", "NUGET_PACKAGES", "NUGET_FALLBACK_PACKAGES", "LD_PRELOAD"):
        env.pop(key, None)
    env["NUGET_PACKAGES"] = str(output / "nuget-cache")
    env["ASTER_CONSUMER_VERSION"] = a.version
    env["ASTER_CONSUMER_FFI_SHA256"] = sha256(ffi)
    env["ASTER_CONSUMER_FFI_PATH"] = str((project / "bin/Release/net9.0/libaster_transport_ffi.so").resolve())
    print(json.dumps({"candidate_package_id": package_id, "candidate_version": a.version, "platform": platform.platform(), "dotnet": dotnet,
        "nupkg": str(nupkg), "nupkg_sha256": sha256(nupkg), "ffi": str(ffi), "ffi_sha256": sha256(ffi),
        "output": str(output), "checkout": str(repo)}, sort_keys=True), flush=True)
    run([dotnet, "run", "--project", "Consumer.csproj", "--configuration", "Release", "--verbosity", "minimal"],
        cwd=project, env=env, log=output / "consumer.log")
    assets = json.loads((project / "obj/project.assets.json").read_text(encoding="utf-8"))
    resolved = [name for name in assets.get("libraries", {}) if name.lower().startswith("aster/")]
    if resolved != [f"Aster/{a.version}"]:
        raise SystemExit(f"restore did not resolve the exact local candidate package: {resolved!r}")
    (output / "result.json").write_text(json.dumps({"result": "PASS", "candidate_package": resolved[0],
        "nupkg_sha256": sha256(nupkg), "ffi_sha256": sha256(ffi), "consumer_project": str(project)}, indent=2) + "\n", encoding="utf-8")
    print(f"DOTNET_CLEAN_CONSUMER_PASS report_dir={output}", flush=True)


if __name__ == "__main__":
    main()
