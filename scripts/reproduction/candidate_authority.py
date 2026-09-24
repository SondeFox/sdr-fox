# SPDX-License-Identifier: MIT OR Apache-2.0
"""Owner-only candidate authority, checked before build/cache side effects."""
import json
import re
import urllib.error
import urllib.request


REPOSITORY = {"full_name": "SondeFox/sdr-fox", "id": 1334845447,
              "node_id": "R_kgDOT5AgBw", "private": True}
OWNER = {"login": "h3lix1", "id": 18344733,
         "node_id": "MDQ6VXNlcjE4MzQ0NzMz", "type": "User"}
WORKFLOW_PATH = ".github/workflows/android-page-size-reproduction.yml"
API_ROOT = "https://api.github.com"
MAX_METADATA_BYTES = 2 * 1024 * 1024


class AuthorityError(ValueError):
    pass


def require(condition, message):
    if not condition:
        raise AuthorityError(message)


def run_identity(env):
    values = []
    for name in ("GITHUB_RUN_ID", "GITHUB_RUN_ATTEMPT"):
        value = env.get(name)
        require(type(value) is str and re.fullmatch(r"[1-9][0-9]*", value) is not None,
                "Candidate run identity unavailable")
        values.append(int(value))
    sha = env.get("GITHUB_SHA")
    require(type(sha) is str and re.fullmatch(r"[0-9a-f]{40}", sha) is not None,
            "Candidate tooling revision unavailable")
    return (*values, sha)


def guard_environment(env):
    expected = {"GITHUB_REPOSITORY": REPOSITORY["full_name"],
                "GITHUB_REPOSITORY_ID": str(REPOSITORY["id"]),
                "GITHUB_ACTOR": OWNER["login"], "GITHUB_ACTOR_ID": str(OWNER["id"]),
                "GITHUB_TRIGGERING_ACTOR": OWNER["login"],
                "GITHUB_REF": "refs/heads/master", "GITHUB_EVENT_NAME": "workflow_dispatch",
                "GITHUB_WORKFLOW_REF": REPOSITORY["full_name"] + "/" + WORKFLOW_PATH + "@refs/heads/master"}
    require(all(env.get(key) == value for key, value in expected.items()), "Candidate owner or repository context mismatch")
    identity = run_identity(env)
    require(env.get("GITHUB_WORKFLOW_SHA") == identity[2], "Candidate workflow revision mismatch")
    return identity


def projection(run_id, attempt, sha):
    return {"schema": 1, "repository": dict(REPOSITORY), "actor": dict(OWNER),
            "triggering_actor": dict(OWNER), "run_id": run_id, "run_attempt": attempt,
            "head_sha": sha, "head_branch": "master", "event": "workflow_dispatch",
            "workflow_path": WORKFLOW_PATH}


def matches(value, expected):
    return type(value) is dict and all(type(value.get(key)) is type(item) and value[key] == item
                                      for key, item in expected.items())


def validate_metadata(env, repository, run):
    run_id, attempt, sha = guard_environment(env)
    require(matches(repository, REPOSITORY | {"default_branch": "master", "archived": False}),
            "Candidate repository identity changed")
    require(matches(run, {"id": run_id, "run_attempt": attempt, "head_sha": sha,
                          "head_branch": "master", "event": "workflow_dispatch", "path": WORKFLOW_PATH}),
            "Candidate current run attempt mismatch")
    for key in ("repository", "head_repository"):
        require(matches(run.get(key), REPOSITORY), "Candidate run repository identity changed")
    for key in ("actor", "triggering_actor"):
        require(matches(run.get(key), OWNER), "Candidate initial or triggering owner identity changed")
    return projection(run_id, attempt, sha)


class NoRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, req, fp, code, msg, headers, newurl):
        raise AuthorityError("Candidate identity metadata redirect refused")


def read_json(path, token):
    # Both paths are constructed below from fixed repository and decimal IDs.
    require(re.fullmatch(r"/repos/SondeFox/sdr-fox(?:/actions/runs/[1-9][0-9]*/attempts/[1-9][0-9]*)?", path) is not None,
            "Unreviewed identity endpoint")
    request = urllib.request.Request(API_ROOT + path, headers={
        "Authorization": "Bearer " + token, "Accept": "application/vnd.github+json",
        "X-GitHub-Api-Version": "2022-11-28", "User-Agent": "SondeFox-native-candidate",
    })
    try:
        with urllib.request.build_opener(NoRedirect()).open(request, timeout=15) as response:
            require(response.status == 200, "Candidate identity metadata unavailable")
            raw = response.read(MAX_METADATA_BYTES + 1)
        require(len(raw) <= MAX_METADATA_BYTES, "Candidate identity metadata oversized")
        def unique(pairs):
            result = {}
            for key, value in pairs:
                require(key not in result, "Candidate identity metadata ambiguous")
                result[key] = value
            return result
        def invalid_number(_):
            raise AuthorityError("Invalid identity number")
        value = json.loads(raw, object_pairs_hook=unique, parse_constant=invalid_number)
        require(type(value) is dict, "Candidate identity metadata invalid")
        return value
    except AuthorityError:
        raise
    except (OSError, urllib.error.URLError, ValueError, RecursionError):
        # Do not include request, token, response body or server error text.
        raise AuthorityError("Candidate identity metadata unavailable") from None


def verify_candidate_authority(env):
    run_id, attempt, _ = guard_environment(env)
    token = env.get("GH_TOKEN")
    require(type(token) is str and bool(token) and not any(c.isspace() for c in token),
            "Candidate read-only job token unavailable")
    repository = read_json("/repos/SondeFox/sdr-fox", token)
    run = read_json(f"/repos/SondeFox/sdr-fox/actions/runs/{run_id}/attempts/{attempt}", token)
    return validate_metadata(env, repository, run)


def verify_receipt_authority(authority, runner):
    require(type(runner) is dict, "Candidate runner identity missing")
    run_id, attempt, sha = run_identity(runner)
    require(type(authority) is dict and
            json.dumps(authority, sort_keys=True, allow_nan=False) == json.dumps(projection(run_id, attempt, sha), sort_keys=True),
            "Candidate receipt authority mismatch")
