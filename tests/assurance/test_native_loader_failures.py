"""Real adverse native loaders; tiny C fixtures, no Aster/Rust compilation.

Run on an Apple Silicon Mac with clang, Node, .NET and JDK22+:
  JAVA_HOME=... python -m unittest discover -s tests/assurance -p test_native_loader_failures.py -v
"""
import hashlib
import json
import os
from pathlib import Path
import platform
import shutil
import subprocess
import sys
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[2]


class NativeLoaderFailures(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        if sys.platform != "darwin" or platform.machine() != "arm64":
            raise unittest.SkipTest("Mach-O adverse fixtures require macOS arm64")
        cls.temp = tempfile.TemporaryDirectory(prefix="aster-loader-")
        cls.addClassCleanup(cls.temp.cleanup)
        cls.work = Path(cls.temp.name)
        cls.records = []
        cls.preparing = True
        cls.fixtures = {}
        versions = {"major": "3 << 16", "minor": "(4 << 16) | (6 << 8)",
                    "missing": None, "compatible": "(4 << 16) | (9 << 8)"}
        for name, value in versions.items():
            source = "int unrelated(void) { return 0; }\n"
            if value: source += f"unsigned int aster_abi_version(void) {{ return {value}; }}\n"
            c = cls.work / f"{name}.c"
            c.write_text(source)
            dylib = cls.work / f"{name}.dylib"
            cls.run_command(["clang", "-dynamiclib", "-arch", "arm64", str(c), "-o", str(dylib)])
            cls.fixtures[name] = dylib
        cls.fixtures["wrongarch"] = cls.work / "wrongarch.dylib"
        cls.run_command(["clang", "-dynamiclib", "-arch", "x86_64", str(cls.work / "compatible.c"), "-o", str(cls.fixtures["wrongarch"])])
        cls.fixtures["absent"] = cls.work / "does-not-exist.dylib"
        c = cls.work / "empty-addon.c"
        c.write_text("void *napi_register_module_v1(void *env, void *exports) { return exports; }\n")
        cls.fixtures["empty-addon"] = cls.work / "empty-addon.node"
        cls.run_command(["clang", "-dynamiclib", "-arch", "arm64", str(c), "-o", str(cls.fixtures["empty-addon"])])
        if not os.environ.get("JAVA_HOME"):
            raise RuntimeError("Set JAVA_HOME to an installed JDK22+ (see tests/assurance/README.md)")
        cls.java = Path(os.environ["JAVA_HOME"]) / "bin/java"
        javac = cls.java.with_name("javac")
        cls.classes = cls.work / "classes"
        cls.classes.mkdir()
        probe = cls.work / "LoaderProbe.java"
        probe.write_text('import site.aster.ffi.IrohLibrary; import java.lang.foreign.FunctionDescriptor; public class LoaderProbe { public static void main(String[] args) { var lib = IrohLibrary.getInstance(); if(args.length > 0) lib.getHandle("aster_runtime_new", FunctionDescriptor.ofVoid()); }}')
        cls.run_command([str(javac), "-cp", str(ROOT / "bindings/java/aster-runtime/target/classes"), "-d", str(cls.classes), str(ROOT / "bindings/java/aster-runtime/src/main/java/site/aster/ffi/IrohLibrary.java"), str(probe)])
        # Compile the current managed binding, never a retained assembly with unknown source.
        cls.managed = cls.work / "managed"
        cls.run_command(["dotnet", "build", str(ROOT / "bindings/dotnet/src/Aster/Aster.csproj"), "--no-restore", "-o", str(cls.managed), "-p:NativeLibraryPath=" + str(cls.work / "empty")])
        probe_dir = cls.work / "dotnet-probe"
        probe_dir.mkdir()
        (probe_dir / "Probe.csproj").write_text('<Project Sdk="Microsoft.NET.Sdk"><PropertyGroup><OutputType>Exe</OutputType><TargetFramework>net9.0</TargetFramework></PropertyGroup><ItemGroup><Reference Include="Aster"><HintPath>' + str(cls.managed / "Aster.dll") + '</HintPath></Reference></ItemGroup></Project>')
        (probe_dir / "Program.cs").write_text('using System; using System.Runtime.InteropServices; NativeLibrary.SetDllImportResolver(typeof(Aster.Runtime).Assembly, (_, _, _) => NativeLibrary.Load(args[0])); using var runtime = new Aster.Runtime();')
        cls.run_command(["dotnet", "build", str(probe_dir / "Probe.csproj"), "-o", str(probe_dir / "out")])
        cls.dotnet_probe = probe_dir / "out/Probe.dll"
        cls.node = shutil.which("node")
        # Transpile the real current SDK loader into a clean consumer sandbox.
        native_source = ROOT / "bindings/typescript/packages/aster/src/native.ts"
        cls.ts_loader = cls.work / "native.mjs"
        transpile = "const fs=require('fs');const ts=require(process.argv[1]);fs.writeFileSync(process.argv[3],ts.transpileModule(fs.readFileSync(process.argv[2],'utf8'),{compilerOptions:{module:ts.ModuleKind.ESNext,target:ts.ScriptTarget.ES2022}}).outputText)"
        cls.run_command([cls.node, "-e", transpile, str(ROOT / "bindings/typescript/node_modules/typescript"), str(native_source), str(cls.ts_loader)])
        cls.preparing = False

    @classmethod
    def run_command(cls, command, env=None):
        result = subprocess.run(command, capture_output=True, text=True, cwd=cls.work, env=env, timeout=60)
        cls.records.append({"command": command, "returncode": result.returncode, "stdout": result.stdout, "stderr": result.stderr})
        if cls.preparing and result.returncode:
            raise RuntimeError(result.stdout + result.stderr)
        return result

    @classmethod
    def tearDownClass(cls):
        evidence = os.environ.get("ASTER_LOADER_EVIDENCE")
        if evidence:
            out = Path(evidence)
            out.mkdir(parents=True, exist_ok=True)
            sources = [ROOT / "bindings/java/aster-runtime/src/main/java/site/aster/ffi/IrohLibrary.java", ROOT / "bindings/dotnet/src/Aster/Runtime.cs", ROOT / "bindings/dotnet/src/Aster/Native.cs", ROOT / "bindings/typescript/packages/aster/src/native.ts", Path(__file__)]
            hashes = {str(p.relative_to(ROOT)): hashlib.sha256(p.read_bytes()).hexdigest() for p in sources}
            fixtures = {name: hashlib.sha256(p.read_bytes()).hexdigest() for name,p in cls.fixtures.items() if p.exists()}
            (out / "loader-results.json").write_text(json.dumps({"platform": platform.platform(), "sources": hashes, "fixtures": fixtures, "commands": cls.records}, indent=2))
        cls.temp.cleanup()

    def assert_failure(self, result, text):
        self.assertNotEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn(text, result.stdout + result.stderr)

    def test_go_real_link_and_abi_variants(self):
        artifact = os.environ.get("ASTER_LOADER_FFI")
        if not artifact:
            self.skipTest("provide ASTER_LOADER_FFI for actual Go reexport/link variants")
        probe = self.work / "go-probe"
        probe.mkdir()
        (probe / "go.mod").write_text('module loaderprobe\ngo 1.25.0\nrequire aster-ffi v0.0.0\nreplace aster-ffi => ' + str(ROOT / "bindings/go") + '\n')
        (probe / "main.go").write_text('package main\nimport ("fmt"; "os"; aster "aster-ffi")\nfunc main() { if err := aster.CheckABI(); err != nil { fmt.Fprintln(os.Stderr,err); os.Exit(2) }; fmt.Println("compatible ABI") }\n')
        for variant in ["major", "minor", "missing", "wrongarch", "absent"]:
            with self.subTest(variant=variant):
                native = self.work / ("go-" + variant)
                native.mkdir()
                lib = native / "libaster_transport_ffi.dylib"
                if variant in ["major", "minor"]:
                    self.run_command(["clang", "-dynamiclib", "-arch", "arm64", str(self.work / (variant + ".c")), "-Wl,-reexport_library," + artifact, "-o", str(lib)])
                elif variant != "absent":
                    shutil.copyfile(self.fixtures[variant], lib)
                env = dict(os.environ, CGO_ENABLED="1", CGO_CFLAGS="-I" + str(ROOT / "ffi"), CGO_LDFLAGS="-L" + str(native) + " -laster_transport_ffi")
                if variant in ["major", "minor"]:
                    env["CGO_LDFLAGS"] += " " + artifact + " -Wl,-rpath," + str(Path(artifact).parent)
                if lib.exists():
                    self.records.append({"go_fixture": variant, "sha256": hashlib.sha256(lib.read_bytes()).hexdigest()})
                binary = probe / ("probe-" + variant)
                build = self.run_command(["go", "-C", str(probe), "build", "-mod=mod", "-o", str(binary), "."], env)
                if variant in ["major", "minor"]:
                    self.assertEqual(build.returncode, 0, build.stdout + build.stderr)
                    result = self.run_command([str(binary)])
                    self.assert_failure(result, "binding expects 4.9 or newer")
                    self.assertIn("native library ABI " + ("3.0.0" if variant == "major" else "4.6.0"), result.stderr)
                else:
                    self.assert_failure(build, "library 'aster_transport_ffi' not found" if variant == "absent" else "_aster_abi_version")
                    if variant == "wrongarch": self.assertIn("x86_64", build.stderr)
        self.records.append({"go_reexport_base": artifact, "sha256": hashlib.sha256(Path(artifact).read_bytes()).hexdigest()})

    def test_java_real_loader_variants(self):
        expected = {"absent": "file does not exist", "major": "has ABI major 3", "minor": "has ABI 4.6", "missing": "has no aster_abi_version symbol", "wrongarch": "incompatible architecture", "compatible": "aster symbol not found: aster_runtime_new"}
        for variant, message in expected.items():
            with self.subTest(variant=variant):
                r = self.run_command([str(self.java), "--enable-native-access=ALL-UNNAMED", "-Daster.ffi.lib=" + str(self.fixtures[variant]), "-cp", str(self.classes) + os.pathsep + str(ROOT / "bindings/java/aster-runtime/target/classes"), "LoaderProbe", "required-symbol"])
                self.assert_failure(r, message)

    def test_dotnet_real_loader_variants(self):
        expected = {"absent": "DllNotFoundException", "major": "does not match binding ABI major 4", "minor": "this binding needs 4.9+", "missing": "EntryPointNotFoundException", "wrongarch": "incompatible architecture", "compatible": "aster_runtime_new"}
        for variant, message in expected.items():
            with self.subTest(variant=variant):
                self.assert_failure(self.run_command(["dotnet", str(self.dotnet_probe), str(self.fixtures[variant])]), message)

    def test_python_extension_loader_variants(self):
        for variant, message in [("absent", "no such file"), ("missing", "PyInit__aster"), ("wrongarch", "incompatible architecture")]:
            with self.subTest(variant=variant):
                script = "import importlib.util; s=importlib.util.spec_from_file_location('_aster', __import__('sys').argv[1]); importlib.util.module_from_spec(s)"
                path = self.work / (variant + '.so')
                if self.fixtures[variant].exists(): shutil.copyfile(self.fixtures[variant], path)
                r = self.run_command([sys.executable, "-c", script, str(path)])
                self.assert_failure(r, message)

    def test_typescript_missing_sdk_exports(self):
        package = self.work / "node_modules/@aster-rpc/transport"
        package.mkdir(parents=True, exist_ok=True)
        shutil.copyfile(self.fixtures["empty-addon"], package / "fixture.node")
        (package / "index.js").write_text("module.exports=require('./fixture.node')")
        # This is a genuinely registered native NAPI addon returning no SDK exports.
        direct = self.run_command([self.node, "-e", "const x=require('./node_modules/@aster-rpc/transport'); console.log(JSON.stringify(Object.keys(x)))"])
        self.assertEqual(direct.returncode, 0, direct.stderr)
        self.assertEqual(direct.stdout.strip(), "[]")
        result = self.run_command([self.node, "--input-type=module", "-e", "import {loadNative} from './native.mjs'; loadNative()"])
        self.assert_failure(result, "Aster native addon (@aster-rpc/transport) is not available")

    def test_typescript_valid_retained_addon(self):
        artifact = os.environ.get("ASTER_LOADER_VALID_ADDON")
        if not artifact:
            self.skipTest("provide ASTER_LOADER_VALID_ADDON for retained native positive control")
        package = self.work / "node_modules/@aster-rpc/transport"
        package.mkdir(parents=True, exist_ok=True)
        shutil.copyfile(artifact, package / "fixture.node")
        (package / "index.js").write_text("module.exports=require('./fixture.node')")
        result = self.run_command([self.node, "--input-type=module", "-e", "import {loadNative} from './native.mjs'; const x=loadNative(); console.log(JSON.stringify({version:x.version(),constructors:['Node','RpcConnection','DispatchServer'].map(k=>typeof x[k])}))"])
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(json.loads(result.stdout)["constructors"], ["function"] * 3)
        self.records.append({"positive_native_sha256": hashlib.sha256(Path(artifact).read_bytes()).hexdigest(), "artifact": artifact})

    def test_typescript_node_loader_variants(self):
        package = self.work / "node_modules/@aster-rpc/transport"
        package.mkdir(parents=True, exist_ok=True)
        for variant in ["absent", "missing", "wrongarch"]:
            with self.subTest(variant=variant):
                nodefile = package / "fixture.node"
                nodefile.unlink(missing_ok=True)
                if self.fixtures[variant].exists(): shutil.copyfile(self.fixtures[variant], nodefile)
                (package / "index.js").write_text("module.exports=require('./fixture.node')")
                r = self.run_command([self.node, "--input-type=module", "-e", "import {loadNative} from './native.mjs'; loadNative()"])
                self.assert_failure(r, "Aster native addon (@aster-rpc/transport) is not available")
                direct = self.run_command([self.node, "-e", "require('./node_modules/@aster-rpc/transport')"])
                detail = {"absent": "MODULE_NOT_FOUND", "missing": "Module did not self-register", "wrongarch": "incompatible architecture"}[variant]
                self.assert_failure(direct, detail)


if __name__ == "__main__":
    unittest.main()
