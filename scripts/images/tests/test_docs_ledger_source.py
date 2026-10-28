"""Exercise the docs workflow's ledger selection against local Git remotes."""

from __future__ import annotations

import os
from pathlib import Path
import subprocess
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[3]
WORKFLOW = ROOT / ".github/workflows/ci-check-docs-image-refs.yml"
BASH = r"C:\Program Files\Git\bin\bash.exe" if os.name == "nt" else "bash"
OFFICIAL_URL = "https://github.com/trycua/cua.git"


def git(directory: Path, *args: str) -> str:
    return subprocess.run(
        ["git", *args], cwd=directory, check=True, capture_output=True, text=True
    ).stdout.strip()


def ledger_step() -> str:
    text = WORKFLOW.read_text(encoding="utf-8")
    section = text.split("      - name: Fetch the ledger\n", 1)[1]
    block = section.split("        run: |\n", 1)[1].split("\n      - name:", 1)[0]
    return "\n".join(
        line[10:] if line.startswith("          ") else line for line in block.splitlines()
    )


class DocsLedgerSourceTests(unittest.TestCase):
    def setUp(self) -> None:
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)
        self.repo = self.root / "repo"
        self.repo.mkdir()
        git(self.repo, "init", "-q")
        self.fork = self.root / "fork.git"
        self.official = self.root / "official.git"
        for remote in (self.fork, self.official):
            git(self.root, "init", "-q", "--bare", str(remote))
        git(self.repo, "remote", "add", "origin", str(self.fork))
        self.runner = self.root / "runner"
        self.runner.mkdir()

    def publish(self, remote: Path, value: str) -> None:
        source = self.root / ("src-" + value)
        source.mkdir()
        git(source, "init", "-q")
        (source / "marker").write_text(value, encoding="utf-8")
        git(source, "add", "marker")
        git(
            source,
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.invalid",
            "commit",
            "-qm",
            value,
        )
        git(source, "push", "-q", str(remote), "HEAD:refs/heads/image-doctor-ledger")

    def run_step(self, repository: str = "loonghao/cua") -> subprocess.CompletedProcess[str]:
        env = os.environ.copy()
        env.update(
            RUNNER_TEMP=self.runner.as_posix(),
            GITHUB_REPOSITORY=repository,
            GIT_CONFIG_COUNT="1",
            GIT_CONFIG_KEY_0=f"url.{self.official.as_uri()}.insteadOf",
            GIT_CONFIG_VALUE_0=OFFICIAL_URL,
        )
        return subprocess.run(
            [BASH, "-e", "-o", "pipefail", "-c", ledger_step()],
            cwd=self.repo,
            env=env,
            capture_output=True,
            text=True,
        )

    def test_missing_fork_ledger_uses_official(self) -> None:
        self.publish(self.official, "official")
        result = self.run_step()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual((self.runner / "ledger/marker").read_text(), "official")
        self.assertIn("trycua/cua", result.stdout)

    def test_existing_fork_ledger_remains_authoritative(self) -> None:
        self.publish(self.official, "official")
        self.publish(self.fork, "fork")
        result = self.run_step()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual((self.runner / "ledger/marker").read_text(), "fork")

    def test_origin_access_error_does_not_fall_back(self) -> None:
        self.publish(self.official, "official")
        git(self.repo, "remote", "set-url", "origin", str(self.root / "missing.git"))
        result = self.run_step()
        self.assertNotEqual(result.returncode, 0)
        self.assertFalse((self.runner / "ledger").exists())

    def test_missing_official_ledger_fails_closed(self) -> None:
        result = self.run_step()
        self.assertNotEqual(result.returncode, 0)
        self.assertFalse((self.runner / "ledger").exists())

    def test_other_repository_keeps_empty_baseline_behavior(self) -> None:
        result = self.run_step("example/other")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(list((self.runner / "ledger").iterdir()), [])


if __name__ == "__main__":
    unittest.main()
