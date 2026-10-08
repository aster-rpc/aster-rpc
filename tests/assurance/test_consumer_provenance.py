"""Source attribution uses an isolated checkout, without loading native SDKs."""
import importlib.util
import pathlib
import subprocess
import tempfile
import unittest


SOURCE = pathlib.Path(__file__).resolve().parents[2] / "scripts/assurance/consumer_smoke.py"
SPEC = importlib.util.spec_from_file_location("consumer_smoke", SOURCE)
consumer = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(consumer)


class ConsumerSourceAttribution(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory()
        self.addCleanup(self.directory.cleanup)
        self.checkout = pathlib.Path(self.directory.name)
        subprocess.run(["git", "init", "-q", str(self.checkout)], check=True)
        subprocess.run([
            "git", "-C", str(self.checkout), "-c", "user.name=Consumer Test",
            "-c", "user.email=consumer-test@example.invalid", "-c", "commit.gpgSign=false", "commit", "-q",
            "--allow-empty", "-m", "isolated harness checkout",
        ], check=True)
        self.head = subprocess.check_output(
            ["git", "-C", str(self.checkout), "rev-parse", "HEAD"], text=True,
        ).strip()

    def test_checkout_is_not_claimed_as_artifact_source_without_candidate_commit(self):
        metadata = consumer.source_metadata(self.checkout, None)
        self.assertIsNone(metadata["candidate_source"])
        self.assertEqual(metadata["harness_checkout_source"], self.head)

    def test_explicit_artifact_source_stays_distinct_from_harness_checkout(self):
        candidate = "7c91964c2443d0c5543a162285bf8ddffc1a7a71"
        self.assertNotEqual(candidate, self.head)
        metadata = consumer.source_metadata(self.checkout, candidate)
        self.assertEqual(metadata["candidate_source"], candidate)
        self.assertEqual(metadata["harness_checkout_source"], self.head)


if __name__ == "__main__":
    unittest.main()
