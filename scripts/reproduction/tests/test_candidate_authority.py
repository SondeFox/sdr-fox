# SPDX-License-Identifier: MIT OR Apache-2.0
"""Candidate authority failures stop before evidence, caches or tool installs."""
import copy
import io
import json
import os
from pathlib import Path
import sys
import tempfile
import unittest
from contextlib import redirect_stderr
from unittest.mock import Mock, patch

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import build_current_pin as build
import candidate_authority as authority
from profiles import ANDROID_PAGE_SIZE, HISTORICAL


TOKEN = "synthetic-test-token-not-a-credential"


def context(attempt=1):
    return {"GITHUB_ACTIONS": "true", "RUNNER_ENVIRONMENT": "github-hosted",
            "RUNNER_OS": "macOS", "RUNNER_ARCH": "ARM64",
            "GITHUB_REPOSITORY": "SondeFox/sdr-fox", "GITHUB_REPOSITORY_ID": "1334845447",
            "GITHUB_ACTOR": "h3lix1", "GITHUB_ACTOR_ID": "18344733",
            "GITHUB_TRIGGERING_ACTOR": "h3lix1", "GITHUB_REF": "refs/heads/master",
            "GITHUB_EVENT_NAME": "workflow_dispatch", "REPRO_PRIVATE_REPOSITORY": "true",
            "GITHUB_WORKFLOW_REF": "SondeFox/sdr-fox/" + authority.WORKFLOW_PATH + "@refs/heads/master",
            "GITHUB_WORKFLOW_SHA": "a" * 40, "GITHUB_SHA": "a" * 40,
            "GITHUB_RUN_ID": "1234", "GITHUB_RUN_ATTEMPT": str(attempt), "GH_TOKEN": TOKEN}


def metadata(attempt=1):
    repository = dict(authority.REPOSITORY, default_branch="master", archived=False)
    run = {"id": 1234, "run_attempt": attempt, "head_sha": "a" * 40, "head_branch": "master",
           "event": "workflow_dispatch", "path": authority.WORKFLOW_PATH,
           "actor": dict(authority.OWNER), "triggering_actor": dict(authority.OWNER),
           "repository": dict(authority.REPOSITORY), "head_repository": dict(authority.REPOSITORY)}
    return repository, run


class CandidateAuthorityTests(unittest.TestCase):
    def test_initial_and_owner_rerun_bind_current_attempt_and_safe_projection(self):
        for attempt in (1, 2):
            with self.subTest(attempt=attempt), patch.object(authority, "read_json", side_effect=metadata(attempt)) as read, patch.object(build.platform, "system", return_value="Darwin"), patch.object(build.platform, "machine", return_value="arm64"):
                result = build.guard_host(context(attempt), ANDROID_PAGE_SIZE)
            self.assertEqual(result, authority.projection(1234, attempt, "a" * 40))
            self.assertEqual([call.args[0] for call in read.call_args_list],
                             ["/repos/SondeFox/sdr-fox", f"/repos/SondeFox/sdr-fox/actions/runs/1234/attempts/{attempt}"])
            self.assertNotIn(TOKEN, json.dumps(result))

    def test_context_drift_rejected_before_any_identity_request(self):
        cases = {"GITHUB_REPOSITORY_ID": "999999", "GITHUB_ACTOR": "another-user",
                 "GITHUB_ACTOR_ID": "999999", "GITHUB_TRIGGERING_ACTOR": "another-user",
                 "GITHUB_SHA": "not-a-sha", "GITHUB_WORKFLOW_SHA": "b" * 40,
                 "GITHUB_RUN_ATTEMPT": "01", "GITHUB_RUN_ID": "", "GITHUB_WORKFLOW_REF": "other.yml"}
        for name, value in cases.items():
            with self.subTest(name=name), patch.object(authority, "read_json") as read, self.assertRaises(authority.AuthorityError):
                authority.verify_candidate_authority(context() | {name: value})
            read.assert_not_called()

    def test_live_repository_id_node_privacy_branch_and_archive_must_match(self):
        for name, value in (("id", 999999), ("node_id", "other-node"), ("private", False),
                            ("private", 1), ("archived", True), ("default_branch", "other")):
            repository, run = metadata()
            repository[name] = value
            with self.subTest(name=name, value=value), self.assertRaises(authority.AuthorityError):
                authority.validate_metadata(context(), repository, run)

    def test_initial_and_triggering_owner_numeric_and_node_identity_checked(self):
        for role in ("actor", "triggering_actor"):
            for name, value in (("id", 999999), ("node_id", "other-node"), ("login", "another-user"), ("type", "Bot")):
                repository, run = metadata(2)
                run[role][name] = value
                with self.subTest(role=role, name=name), self.assertRaises(authority.AuthorityError):
                    authority.validate_metadata(context(2), repository, run)

    def test_current_attempt_source_workflow_and_nested_repository_must_match(self):
        for name, value in (("run_attempt", 1), ("id", 1235), ("head_sha", "b" * 40),
                            ("event", "pull_request"), ("head_branch", "another"), ("path", "another.yml")):
            repository, run = metadata(2)
            run[name] = value
            with self.subTest(name=name), self.assertRaises(authority.AuthorityError):
                authority.validate_metadata(context(2), repository, run)
        for name in ("repository", "head_repository"):
            repository, run = metadata()
            run[name]["id"] = 999999
            with self.subTest(name=name), self.assertRaises(authority.AuthorityError):
                authority.validate_metadata(context(), repository, run)

    def test_missing_unavailable_and_incomplete_metadata_fail_closed(self):
        for name in ("GH_TOKEN", "GITHUB_ACTOR_ID", "GITHUB_TRIGGERING_ACTOR", "GITHUB_RUN_ATTEMPT"):
            env = context()
            env.pop(name)
            with self.subTest(name=name), patch.object(authority, "read_json") as read, self.assertRaises(authority.AuthorityError):
                authority.verify_candidate_authority(env)
            read.assert_not_called()
        with patch.object(authority, "read_json", side_effect=authority.AuthorityError("metadata unavailable")), self.assertRaises(authority.AuthorityError):
            authority.verify_candidate_authority(context())
        repository, run = metadata()
        for name in ("actor", "triggering_actor", "run_attempt", "repository"):
            altered = copy.deepcopy(run)
            altered.pop(name)
            with self.subTest(missing=name), self.assertRaises(authority.AuthorityError):
                authority.validate_metadata(context(), repository, altered)

    def test_cli_refuses_nonowner_rerun_or_unavailable_metadata_before_builder(self):
        wrong_repository, valid_run = metadata()
        wrong_repository["node_id"] = "different-repository"
        valid_repository, wrong_rerun = metadata()
        wrong_rerun["triggering_actor"]["id"] = 999999
        cases = [(context() | {key: value}, None) for key, value in
                 (("GITHUB_ACTOR", "another-user"), ("GITHUB_ACTOR_ID", "999999"),
                  ("GITHUB_TRIGGERING_ACTOR", "another-user"), ("GITHUB_REPOSITORY_ID", "999999"))]
        cases += [(context(), authority.AuthorityError("metadata unavailable")),
                  (context(), [wrong_repository, valid_run]),
                  (context(), [valid_repository, wrong_rerun]), (context(2), list(metadata(1)))]
        for altered, failure in cases:
            with tempfile.TemporaryDirectory() as directory:
                evidence = Path(directory) / "evidence"
                argv = ["build_current_pin.py", "--profile", ANDROID_PAGE_SIZE.name,
                        "--source-dir", directory, "--evidence-dir", str(evidence)]
                with patch.dict(os.environ, altered, clear=True), patch.object(sys, "argv", argv), patch.object(build.platform, "system", return_value="Darwin"), patch.object(build.platform, "machine", return_value="arm64"), patch.object(authority, "read_json", side_effect=failure), patch.object(build, "Builder") as builder, redirect_stderr(io.StringIO()):
                    self.assertEqual(build.main(), 1)
                builder.assert_not_called()
                self.assertFalse(evidence.exists())

    def test_job_token_is_not_in_build_environment_or_receipt(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            worker = build.Builder(root, root / "evidence", context(), ANDROID_PAGE_SIZE)
            self.assertNotIn("GH_TOKEN", worker.env)
            self.assertNotIn(TOKEN, json.dumps(worker.receipt))

    def test_receipt_authority_binds_runner_and_rejects_extra_or_wrong_types(self):
        env = context()
        bound = authority.projection(1234, 1, "a" * 40)
        authority.verify_receipt_authority(bound, env)
        for altered in (bound | {"run_attempt": True}, bound | {"token": TOKEN}, bound | {"run_attempt": 2}):
            with self.subTest(keys=list(altered)), self.assertRaises(authority.AuthorityError):
                authority.verify_receipt_authority(altered, env)

    def test_historical_host_guard_does_not_acquire_new_authority_requirement(self):
        env = context() | {"GITHUB_ACTOR": "historical-context", "GITHUB_TRIGGERING_ACTOR": "historical-context"}
        for name in ("GH_TOKEN", "GITHUB_ACTOR_ID", "GITHUB_REPOSITORY_ID"):
            env.pop(name)
        with patch.object(authority, "read_json") as read, patch.object(build.platform, "system", return_value="Darwin"), patch.object(build.platform, "machine", return_value="arm64"):
            self.assertIsNone(build.guard_host(env, HISTORICAL))
        read.assert_not_called()

    def test_http_failures_and_redirects_do_not_disclose_token_or_response(self):
        opener = Mock()
        opener.open.side_effect = OSError("server included " + TOKEN)
        with patch.object(authority.urllib.request, "build_opener", return_value=opener), self.assertRaises(authority.AuthorityError) as failure:
            authority.read_json("/repos/SondeFox/sdr-fox", TOKEN)
        self.assertNotIn(TOKEN, str(failure.exception))
        with self.assertRaises(authority.AuthorityError):
            authority.NoRedirect().redirect_request(None, None, 302, "", {}, "https://other.invalid")
        with patch.object(authority.urllib.request, "build_opener") as open_, self.assertRaises(authority.AuthorityError):
            authority.read_json("/repos/SondeFox/sdr-fox-other", TOKEN)
        open_.assert_not_called()

    def test_metadata_bytes_are_bounded_and_duplicate_keys_rejected(self):
        for raw in (b'{"id":1,"id":2}', b'x' * (authority.MAX_METADATA_BYTES + 1), b'[]'):
            response = Mock(status=200)
            response.read.return_value = raw
            opener = Mock()
            opener.open.return_value.__enter__ = Mock(return_value=response)
            opener.open.return_value.__exit__ = Mock(return_value=False)
            with self.subTest(size=len(raw)), patch.object(authority.urllib.request, "build_opener", return_value=opener), self.assertRaises(authority.AuthorityError):
                authority.read_json("/repos/SondeFox/sdr-fox", TOKEN)


if __name__ == "__main__":
    unittest.main()
