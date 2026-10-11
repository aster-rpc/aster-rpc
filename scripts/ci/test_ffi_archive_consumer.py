"""Final archive inventory/checksum and isolated-consumer regressions."""
import importlib.util
import io
import json
import os
from pathlib import Path
import subprocess
import tarfile
import tempfile
import unittest
from unittest.mock import patch

ROOT = Path(__file__).resolve().parents[2]
SPEC = importlib.util.spec_from_file_location("ffi_archive", ROOT / "scripts/assurance/ffi_archive_consumer.py")
F = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(F)


class ArchiveConsumer(unittest.TestCase):
    def test_windows_and_space_paths_quote_whole_cgo_arguments(self):
        for native in (Path(r"C:\Users\runner\AppData\Local\Temp\native"), Path("/tmp/consumer with spaces/native")):
            cflags, ldflags = F.cgo_flags(native, "win-x64")
            self.assertEqual(cflags, "'" + "-I" + str(native) + "'")
            self.assertTrue(ldflags.startswith("'" + "-L" + str(native) + "' "))
            self.assertNotIn("rpath", ldflags)

    def test_native_host_and_emulator_target_cannot_be_mislabeled(self):
        with patch.object(F.platform, "machine", return_value="x86_64"), patch.object(F.platform, "system", return_value="Linux"):
            F.verify_host("linux-x64", False)
            F.verify_host("linux-arm64", True)
            for rid, qemu in (("linux-arm64", False), ("win-x64", False), ("linux-x64", True)):
                with self.subTest(rid=rid, qemu=qemu), self.assertRaises(ValueError):
                    F.verify_host(rid, qemu)

    def archive(self, root, members=None):
        path = root / "libaster_transport_ffi-linux-x64.tar.gz"
        with tarfile.open(path, "w:gz") as bundle:
            for name, kind in members or [("./aster_ffi.h", "file"), ("./libaster_transport_ffi.so", "file")]:
                member = tarfile.TarInfo(name)
                if kind == "link":
                    member.type = tarfile.SYMTYPE
                    member.linkname = "/etc/passwd"
                    bundle.addfile(member)
                else:
                    member.size = 4
                    bundle.addfile(member, io.BytesIO(b"test"))
        (root / "FFI-SHA256SUMS").write_text(F.digest(path) + "  " + path.name + "\n")
        return path

    def test_rejects_missing_header_extra_file_traversal_link_and_duplicate(self):
        inventories = [
            [("libaster_transport_ffi.so", "file")],
            [("aster_ffi.h", "file"), ("libaster_transport_ffi.so", "file"), ("surprise", "file")],
            [("../aster_ffi.h", "file"), ("libaster_transport_ffi.so", "file")],
            [("aster_ffi.h", "link"), ("libaster_transport_ffi.so", "file")],
            [("aster_ffi.h", "file"), ("./aster_ffi.h", "file"), ("libaster_transport_ffi.so", "file")],
        ]
        for inventory in inventories:
            with self.subTest(inventory=inventory), tempfile.TemporaryDirectory() as directory:
                root = Path(directory)
                with self.assertRaises(ValueError):
                    F.extract(self.archive(root, inventory), root / "extract", F.LIBRARIES["linux-x64"])

    def test_checksum_failure_precedes_compiler(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            archive = self.archive(root)
            (root / "FFI-SHA256SUMS").write_text("0" * 64 + "  " + archive.name + "\n")
            with patch.object(F.subprocess, "run") as invoke, self.assertRaises(ValueError):
                F.run(archive, "linux-x64", "0.3.1", root / "evidence")
            invoke.assert_not_called()

    def test_isolation_and_wrong_loaded_image_rejection(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            archive = self.archive(root)
            calls = []

            def invoke(argv, **kwargs):
                calls.append(argv)
                project = kwargs["cwd"]
                env = kwargs["env"]
                self.assertNotIn("CPATH", env)
                self.assertNotIn("LD_PRELOAD", env)
                self.assertEqual(env["GOWORK"], "off")
                self.assertNotIn(str(ROOT), env["CGO_CFLAGS"])
                self.assertIn(str(project / "native"), env["CGO_CFLAGS"])
                self.assertNotIn("replace", (project / "go.mod").read_text())
                receipt = {"version": "0.3.1", "loaded_image": "/wrong/libaster_transport_ffi.so"}
                return subprocess.CompletedProcess(argv, 0, json.dumps(receipt), "")

            with patch.dict(os.environ, {"CPATH": str(ROOT / "ffi"), "LD_PRELOAD": "/wrong.so"}), patch.object(F.subprocess, "run", side_effect=invoke):
                with self.assertRaises(ValueError):
                    F.run(archive, "linux-x64", "0.3.1", root / "evidence")
            self.assertEqual(len(calls), 2)
            self.assertFalse((root / "evidence/receipt.json").exists())

    def test_workflow_gate_is_required_for_sealing(self):
        policy = json.loads((ROOT / "scripts/ci/release-candidate-policy.json").read_text())
        self.assertIn("ffi-archive-consumers", policy["required_jobs"])
        workflow = (ROOT / ".forgejo/workflows/linux.yml").read_text()
        gate = workflow.split("  ffi-archive-consumers:\n")[1].split("  candidate:\n")[0]
        for rid in F.LIBRARIES:
            self.assertIn("rid: " + rid, gate)
        self.assertIn("needs: ffi-package", gate)
        self.assertIn("--fetch", gate)
        self.assertIn("--qemu", gate)
        self.assertIn("ffi-archive-consumers", workflow.split("  candidate:\n")[1])


if __name__ == "__main__":
    unittest.main()
