"""Release script tests that need no credentials or network."""

import hashlib
import importlib.util
import os
import pathlib
import subprocess
import tarfile
import tempfile
import unittest

RELEASE = pathlib.Path(__file__).resolve().parents[1]

spec = importlib.util.spec_from_file_location("next_version", RELEASE / "next-version.py")
next_version = importlib.util.module_from_spec(spec)
spec.loader.exec_module(next_version)


class NextVersion(unittest.TestCase):
    def test_counts_within_a_day_and_restarts_on_a_new_one(self):
        tags = ["refs/tags/v20261009.1", "v20261009.2", "v20261008.7", "nightly", "v1.2.3"]
        self.assertEqual(next_version.next_version("20261009", tags), "20261009.3")
        self.assertEqual(next_version.next_version("20261010", tags), "20261010.1")
        self.assertEqual(next_version.next_version("20261009", []), "20261009.1")

    def test_refuses_a_date_older_than_a_published_tag(self):
        with self.assertRaises(ValueError):
            next_version.next_version("20261008", ["v20261009.1"])


def run(*args, **kwargs):
    return subprocess.run(args, check=True, capture_output=True, text=True, **kwargs)


class Packaging(unittest.TestCase):
    def test_archive_holds_the_binary_and_docs_at_fixed_paths(self):
        with tempfile.TemporaryDirectory() as temp:
            executable = pathlib.Path(temp, "agent-launcher")
            executable.write_text("#!/bin/sh\n")
            executable.chmod(0o755)
            notices = pathlib.Path(temp, "notices.txt")
            notices.write_text("Third-party notices\n")
            archive = run("bash", RELEASE / "package.sh", "20261009.1",
                          "x86_64-unknown-linux-gnu", executable, temp, notices).stdout.strip()
            prefix = "agent-launcher-20261009.1-x86_64-unknown-linux-gnu/"
            with tarfile.open(archive) as tar:
                members = {member.name: member for member in tar.getmembers()}
            self.assertEqual(members[prefix + "bin/agent-launcher"].mode & 0o777, 0o755)
            for name in ("README.md", "LICENSE", "NOTICE", "THIRD-PARTY-NOTICES.txt",
                         "config.example.toml"):
                self.assertIn(prefix + name, members)
            self.assertTrue(all(member.uid == 0 for member in members.values()))


class Formula(unittest.TestCase):
    def test_renders_every_platform_checksum_from_sha256sums(self):
        version = "20261009.1"
        sums = {
            target: hashlib.sha256(target.encode()).hexdigest()
            for target in ("universal-apple-darwin", "x86_64-unknown-linux-gnu",
                           "aarch64-unknown-linux-gnu")
        }
        with tempfile.TemporaryDirectory() as temp:
            path = pathlib.Path(temp, "SHA256SUMS")
            path.write_text("".join(
                f"{digest}  agent-launcher-{version}-{target}.tar.gz\n"
                for target, digest in sums.items()))
            formula = run("bash", RELEASE / "render-formula.sh", version, path).stdout
        self.assertIn(f'  version "{version}"', formula)
        self.assertNotIn("@", formula.replace("@users", ""))
        for target, digest in sums.items():
            self.assertIn(f"v{version}/agent-launcher-{version}-{target}.tar.gz", formula)
            self.assertIn(f'sha256 "{digest}"', formula)

    def test_refuses_a_missing_checksum(self):
        with tempfile.TemporaryDirectory() as temp:
            path = pathlib.Path(temp, "SHA256SUMS")
            path.write_text("")
            result = subprocess.run(["bash", RELEASE / "render-formula.sh", "20261009.1", path],
                                    capture_output=True, text=True)
        self.assertNotEqual(result.returncode, 0)


if __name__ == "__main__":
    unittest.main()
