from __future__ import annotations

import base64
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
from threading import Thread
from urllib.parse import quote

import pytest


ROOT = Path(__file__).resolve().parents[3]
CONFIG = ".github/release-attribution-config.json"
ANCESTOR = "a" * 40
HEAD = "b" * 40
TARGET = "c" * 40
DEFAULT_ANCESTOR = "d" * 40
KNOWN = {"known@institution.example": "known-author"}
UNSET = object()


def contents(overrides):
    if overrides is None:
        return None
    return json_contents({"identityOverrides": overrides})


def json_contents(value):
    if value is None:
        return None
    data = json.dumps(value).encode()
    return {"encoding": "base64", "content": base64.b64encode(data).decode()}


@pytest.fixture
def github_api():
    responses = {}

    class Handler(BaseHTTPRequestHandler):
        def log_message(self, *_args):
            pass

        def do_GET(self):
            payload = responses.get(self.path)
            data = json.dumps(payload).encode()
            self.send_response(404 if payload is None else 200)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(data)))
            self.end_headers()
            self.wfile.write(data)

    server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    thread = Thread(target=server.serve_forever, daemon=True)
    thread.start()
    try:
        yield f"http://127.0.0.1:{server.server_port}", responses
    finally:
        server.shutdown()
        server.server_close()
        thread.join(timeout=5)


@pytest.fixture
def validate_command(tmp_path, github_api):
    api_url, responses = github_api
    git_executable = shutil.which("git")
    assert git_executable is not None, "the command fixtures require Git"
    env = {
        "HOME": str(tmp_path),
        "PATH": str(Path(git_executable).parent) + os.pathsep + os.defpath,
        "GIT_CONFIG_NOSYSTEM": "1",
        "GH_TOKEN": "local-fixture-token",
        "GITHUB_API_URL": api_url,
    }
    if os.name == "nt":
        env["SystemRoot"] = os.environ["SystemRoot"]

    def git(*args):
        return subprocess.check_output(
            ["git", *args], cwd=tmp_path, env=env, text=True
        ).strip()

    def validate(
        *, ancestor, head, body="", merge_base_sha=ANCESTOR,
        target_config=UNSET, base_metadata=None, reference=UNSET,
        default_ancestor=UNSET, head_policy=None, head_sha=HEAD,
        author_login="landing-author", coauthor_email="known@institution.example",
    ):
        config = tmp_path / CONFIG
        config.parent.mkdir()
        config.write_text(json.dumps({"identityOverrides": KNOWN}))
        git("init", "-q", "--template=")
        git("add", CONFIG)
        git(
            "-c", "user.name=Fixture",
            "-c", "user.email=fixture@example.invalid",
            "commit", "-qm", "trusted policy",
        )
        trusted_sha = git("rev-parse", "HEAD")
        base = {"sha": TARGET, "ref": "candidate", "repo": {"full_name": "trycua/cua"}}
        if base_metadata is not None:
            base.update(base_metadata)
        if reference is UNSET:
            reference = {"ref": f"refs/heads/{base['ref']}", "object": {"type": "commit", "sha": TARGET}}
        if target_config is UNSET:
            target_config = {"identityOverrides": KNOWN}
        head_config = None if head is None else {"identityOverrides": head, **(head_policy or {})}
        (tmp_path / "event.json").write_text(json.dumps({
            "repository": {"full_name": "trycua/cua"},
            "pull_request": {"number": 50},
        }))
        responses.update({
            "/repos/trycua/cua/pulls/50": {
                "number": 50,
                "user": {"login": "landing-author"},
                "body": body,
                "commits": 1,
                "base": base,
                "head": {"sha": head_sha, "repo": {"full_name": "contributor/cua"}},
            },
            "/repos/trycua/cua/pulls/50/commits?per_page=100&page=1": [{
                "sha": HEAD,
                "author": {"login": author_login},
                "committer": {"login": "landing-author"},
                "commit": {
                    "author": {
                        "name": "Landing Author",
                        "email": f"123+{author_login}@users.noreply.github.com",
                    },
                    "message": f"docs: update guide\n\nCo-authored-by: Known <{coauthor_email}>",
                },
            }],
            f"/repos/contributor/cua/contents/{CONFIG}?ref={head_sha}": json_contents(head_config),
            f"/repos/trycua/cua/git/ref/heads/{quote(str(base.get('ref') or ''), safe='')}": reference,
            f"/repos/trycua/cua/contents/{CONFIG}?ref={TARGET}": json_contents(target_config),
            f"/repos/trycua/cua/compare/{trusted_sha}...{head_sha}?per_page=1": {
                "merge_base_commit": {"sha": DEFAULT_ANCESTOR if merge_base_sha else ""},
            },
            f"/repos/trycua/cua/contents/{CONFIG}?ref={DEFAULT_ANCESTOR}": contents(
                ancestor if default_ancestor is UNSET else default_ancestor
            ),
            f"/repos/trycua/cua/compare/{TARGET}...{head_sha}?per_page=1": {
                "merge_base_commit": {"sha": merge_base_sha},
            },
            f"/repos/trycua/cua/contents/{CONFIG}?ref={ANCESTOR}": contents(ancestor),
        })
        return subprocess.run(
            [
                sys.executable, str(ROOT / ".github/scripts/release_attribution.py"),
                "validate-pr", "--event", "event.json",
            ],
            cwd=tmp_path,
            env=env,
            capture_output=True,
            text=True,
            timeout=30,
        )

    return validate


@pytest.mark.parametrize(
    "ancestor",
    [
        {},
        {"known@institution.example": "previous-author"},
        {"retired@institution.example": "retired-author"},
        KNOWN,
    ],
    ids=["before-addition", "before-update", "before-removal", "current"],
)
def test_unchanged_configuration_uses_current_trusted_policy(validate_command, ancestor):
    result = validate_command(ancestor=ancestor, head=ancestor)

    assert result.returncode == 0, result.stdout + result.stderr
    assert "merge-ready for pull request #50" in result.stdout


@pytest.mark.parametrize(
    "head", [{}, {"known@institution.example": "other-author"}], ids=["removed", "altered"]
)
def test_actual_changes_to_trusted_mappings_are_rejected(validate_command, head):
    result = validate_command(ancestor=KNOWN, head=head)

    assert result.returncode == 1
    assert "removes or changes trusted identityOverrides" in result.stderr
    assert "merge-ready" not in result.stdout


def test_unverified_addition_on_stale_branch_is_rejected(validate_command):
    result = validate_command(ancestor={}, head={"new@institution.example": "new-author"})

    assert result.returncode == 1
    assert "new@institution.example" in result.stderr
    assert "has no explicit same-repository source PR" in result.stderr
    assert "merge-ready" not in result.stdout


@pytest.mark.parametrize("login,exit_code", [("source-author", 0), ("another-author", 1)])
def test_addition_on_stale_branch_requires_verified_login(
    validate_command, github_api, login, exit_code
):
    _, responses = github_api
    responses["/repos/trycua/cua/pulls/12"] = {
        "number": 12, "user": {"login": "source-author"},
    }
    responses["/repos/trycua/cua/pulls/12/commits?per_page=100&page=1"] = [
        {"commit": {"author": {"email": "source@institution.example"}}},
    ]
    result = validate_command(
        ancestor={},
        head={"source@institution.example": login},
        body="The identity is verified by https://github.com/trycua/cua/pull/12",
    )

    assert result.returncode == exit_code, result.stdout + result.stderr
    if exit_code:
        assert "verified source PR author is @source-author" in result.stderr
        assert "merge-ready" not in result.stdout
    else:
        assert "merge-ready for pull request #50" in result.stdout


@pytest.mark.parametrize("missing", ["ancestor", "head"])
def test_unreadable_configuration_is_not_approved(validate_command, missing):
    configurations = {"ancestor": {}, "head": {}}
    configurations[missing] = None
    result = validate_command(**configurations)

    assert result.returncode == 1
    assert "GitHub API GET" in result.stderr
    assert "404" in result.stderr
    assert "merge-ready" not in result.stdout


def test_missing_merge_base_is_not_approved(validate_command):
    result = validate_command(ancestor={}, head={}, merge_base_sha="")

    assert result.returncode == 1
    assert "no merge base" in result.stderr
    assert "merge-ready" not in result.stdout


def test_actual_target_inherited_overrides_are_not_new_additions(validate_command):
    inherited = {
        **KNOWN,
        "upstream-one@institution.example": "upstream-one-author",
        "upstream-two@institution.example": "upstream-two-author",
    }
    result = validate_command(
        ancestor=inherited, head=inherited,
        target_config={"identityOverrides": inherited}, default_ancestor=KNOWN,
    )

    assert result.returncode == 0, result.stdout + result.stderr
    assert "merge-ready for pull request #50" in result.stdout


def test_head_configuration_cannot_be_its_own_baseline(validate_command):
    result = validate_command(
        ancestor=KNOWN,
        head={**KNOWN, "unverified@institution.example": "unverified-author"},
        target_config={"identityOverrides": KNOWN},
    )

    assert result.returncode == 1
    assert "unverified@institution.example" in result.stderr
    assert "has no explicit same-repository source PR" in result.stderr
    assert "merge-ready" not in result.stdout


def test_missing_target_reference_is_not_approved(validate_command):
    result = validate_command(ancestor=KNOWN, head=KNOWN, reference=None)

    assert result.returncode == 1
    assert "GitHub API GET" in result.stderr
    assert "404" in result.stderr
    assert "merge-ready" not in result.stdout


@pytest.mark.parametrize("changed", [None, "another-author"], ids=["removed", "altered"])
def test_target_only_inherited_mapping_cannot_be_changed(validate_command, changed):
    inherited = {**KNOWN, "upstream@institution.example": "upstream-author"}
    head = dict(inherited)
    if changed is None:
        del head["upstream@institution.example"]
    else:
        head["upstream@institution.example"] = changed
    result = validate_command(ancestor=inherited, head=head, target_config={"identityOverrides": inherited})

    assert result.returncode == 1
    assert "removes or changes trusted identityOverrides" in result.stderr


def test_current_default_and_target_mapping_conflict_is_rejected(validate_command):
    result = validate_command(
        ancestor=KNOWN, head=KNOWN,
        target_config={"identityOverrides": {"known@institution.example": "another-author"}},
    )

    assert result.returncode == 1
    assert "target identityOverrides conflict with trusted policy" in result.stderr


@pytest.mark.parametrize("source", ["target", "head"])
@pytest.mark.parametrize(
    "policy,author_login,coauthor_email",
    [
        ({"bots": ["preserved-author"]}, "preserved-author", "known@institution.example"),
        ({"internalHandles": ["preserved-author"]}, "preserved-author", "known@institution.example"),
        ({"optOutHandles": ["preserved-author"]}, "preserved-author", "known@institution.example"),
        ({"ignoredCoauthorEmails": ["unknown@institution.example"]}, "landing-author", "unknown@institution.example"),
        ({"coauthorOverrides": {"Known <unknown@institution.example>": "landing-author"}}, "landing-author", "unknown@institution.example"),
    ],
    ids=["bots", "internal", "opt-out", "ignored-email", "coauthor-override"],
)
def test_non_identity_policy_remains_trusted_default(
    validate_command, source, policy, author_login, coauthor_email
):
    result = validate_command(
        ancestor=KNOWN, head=KNOWN, author_login=author_login, coauthor_email=coauthor_email,
        target_config={"identityOverrides": KNOWN, **(policy if source == "target" else {})},
        head_policy=policy if source == "head" else {},
    )

    assert result.returncode == 1
    assert "contributor attribution is not merge-ready" in result.stderr


@pytest.mark.parametrize(
    "metadata,error",
    [
        ({"repo": {"full_name": "foreign/cua"}}, "target repository"),
        ({"repo": {}}, "target repository"),
        ({"ref": ""}, "target branch ref"),
        ({"ref": "../candidate"}, "target branch ref"),
    ],
    ids=["foreign-repo", "missing-repo", "missing-ref", "invalid-ref"],
)
def test_invalid_actual_target_metadata_is_rejected(validate_command, metadata, error):
    result = validate_command(ancestor=KNOWN, head=KNOWN, base_metadata=metadata)

    assert result.returncode == 1
    assert error in result.stderr


@pytest.mark.parametrize(
    "reference",
    [
        {"ref": "refs/heads/other", "object": {"type": "commit", "sha": TARGET}},
        {"ref": "refs/heads/candidate", "object": {"type": "tag", "sha": TARGET}},
        {"ref": "refs/heads/candidate", "object": {"type": "commit", "sha": "c" * 7}},
        {"ref": "refs/heads/candidate", "object": {"type": "commit", "sha": "z" * 40}},
    ],
    ids=["ref-mismatch", "non-commit", "short-sha", "nonhex-sha"],
)
def test_unverified_target_tip_is_rejected(validate_command, reference):
    result = validate_command(ancestor=KNOWN, head=KNOWN, reference=reference)

    assert result.returncode == 1
    assert "invalid target branch commit" in result.stderr


@pytest.mark.parametrize(
    "target,error",
    [
        (None, "404"), ([], "must contain a JSON object"),
        ({}, "must contain identityOverrides"),
        ({"identityOverrides": []}, "must contain identityOverrides"),
    ],
    ids=["missing-file", "non-object", "missing-overrides", "non-object-overrides"],
)
def test_unreadable_target_policy_is_not_approved(validate_command, target, error):
    result = validate_command(ancestor=KNOWN, head=KNOWN, target_config=target)

    assert result.returncode == 1
    assert error in result.stderr


def test_invalid_head_commit_sha_is_rejected(validate_command):
    result = validate_command(ancestor=KNOWN, head=KNOWN, head_sha="b" * 7)

    assert result.returncode == 1
    assert "full head commit SHA" in result.stderr
