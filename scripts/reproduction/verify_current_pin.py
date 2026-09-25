#!/usr/bin/env python3
"""Fail-closed inspection of the five current consumer artifacts.

This checks bytes and build inputs. It cannot establish that a build was fresh;
that claim additionally requires the separately reviewed workflow/run receipt.
Only fixed labels, hashes, counts, and allowlisted tool identities are emitted.
"""
from __future__ import annotations

import argparse
import collections
import hashlib
import json
import os
from pathlib import Path
import platform
import re
import shlex
import shutil
import struct
import subprocess
import sys
import tempfile
import tomllib

sys.path.insert(0, str(Path(__file__).resolve().parent))
from profiles import HISTORICAL, PROFILES, build_override_args, load_manifest, select_profile
from page_layout import check_link_args, inspect_layout
from candidate_authority import verify_receipt_authority
from host_build_policy import inspect_macos_build, verify_target_strip

EXPECTED_PATH = Path(__file__).with_name("expected.json")
CANONICAL_SOURCE_URLS = frozenset({
    "https://github.com/SondeFox/sdr-fox",
    "https://github.com/SondeFox/sdr-fox.git",
})

# Exact official Rust runtime input, not a textual path-prefix exemption.
COMPILER_BUILTINS_TOOLCHAIN = "1.95.0-aarch64-apple-darwin"
COMPILER_BUILTINS_RUSTC_VERSION = "rustc 1.95.0 (59807616e 2026-04-14)"
COMPILER_BUILTINS_RUSTC_SHA256 = "b829b733131d4e1673eeebd1f34d06ae1e9ff4977b051313cf42e2a9e79ecf1c"
COMPILER_BUILTINS_RELATIVE_PATH = "lib/rustlib/aarch64-apple-darwin/lib/libcompiler_builtins-da5ac53f4a183f75.rlib"
COMPILER_BUILTINS_SHA256 = "10c965331110c1c53556eab71fe6db80f199a71c1f746d68eafba96f4ac841cb"
COMPILER_BUILTINS_SIZE = 3291616
AR_SYMBOL_NAMES = frozenset({"/", "__.SYMDEF", "__.SYMDEF SORTED", "__.SYMDEF_64", "__.SYMDEF_64 SORTED"})


class VerificationError(Exception):
    """Contains a fixed code and allowlisted observations, never raw output."""

    def __init__(self, code, observation=None):
        super().__init__(code)
        self.observation = observation


def require(condition, code, observation=None):
    if not condition:
        raise VerificationError(code, observation)


def sha256(data):
    return hashlib.sha256(data).hexdigest()


def run(argv, cwd=None, *, timeout=180):
    try:
        result = subprocess.run(argv, cwd=cwd, capture_output=True, text=True,
                                encoding="utf-8", errors="strict", timeout=timeout)
    except (OSError, subprocess.TimeoutExpired, UnicodeError):
        raise VerificationError("inspection-command-unavailable") from None
    command = Path(argv[0]).name
    command = command if command in {"git", "cargo", "rustc", "lipo", "nm", "otool", "llvm-readelf", "llvm-nm", "sw_vers", "xcrun", "xcodebuild"} else "inspection-tool"
    require(result.returncode == 0, "inspection-command-failed", {"command": command, "exit_code": result.returncode})
    require(bool(result.stdout.strip()), "inspection-command-empty")
    return result.stdout


def git(source, *args, allow_empty=False, strip_output=True):
    argv = ["git", "--no-replace-objects", "-c", "core.fsmonitor=false", "-c", "core.hooksPath=/dev/null", "-C", str(source), *args]
    if allow_empty or not strip_output:
        try:
            # Decode bytes ourselves so universal-newline conversion cannot
            # turn a configured trailing CR into the command's sole LF.
            result = subprocess.run(argv, capture_output=True, timeout=30)
            output = result.stdout.decode("utf-8")
        except (OSError, subprocess.TimeoutExpired, UnicodeError):
            raise VerificationError("source-command-unavailable") from None
        require(result.returncode == 0, "source-command-failed")
    else:
        output = run(argv)
    return output.strip() if strip_output else output


def safe_file(root, relative):
    """Inputs must be ordinary files at their exact relative names."""
    p = Path(relative)
    require(not p.is_absolute() and p.parts and all(x not in (".", "..") for x in p.parts), "unsafe-input-name")
    result = root / p
    cursor = root
    for part in p.parts:
        cursor = cursor / part
        require(not cursor.is_symlink(), "symlink-input")
    require(result.is_file(), "input-file-missing")
    require(result.resolve().is_relative_to(root.resolve()), "input-outside-root")
    return result


def validate_environment(source, env):
    # Git and Cargo can silently redirect the source/configuration being checked.
    bad = {k for k in env if k.startswith(("GIT_CONFIG", "GIT_OBJECT", "GIT_ALTERNATE", "CARGO_PROFILE_", "CARGO_TARGET_", "CARGO_BUILD_", "CARGO_REGISTRIES_", "CARGO_SOURCE_"))}
    bad.update(k for k in ("GIT_DIR", "GIT_WORK_TREE", "GIT_COMMON_DIR", "GIT_INDEX_FILE", "CARGO_HOME", "RUSTC_WRAPPER", "RUSTC_WORKSPACE_WRAPPER", "RUSTC", "CARGO_ENCODED_RUSTFLAGS", "RUST_TARGET_PATH") if k in env)
    require(not bad, "redirecting-environment")
    home = Path(env.get("HOME", ""))
    require(home.is_absolute() and home.is_dir(), "invalid-home")
    cargo_home = (home / ".cargo").resolve()
    expected_flags = f"--remap-path-prefix={source}=/workspace/sdr-fox --remap-path-prefix={cargo_home}=/cargo --remap-path-prefix={home}=/home/builder"
    require("RUSTFLAGS" not in env or env["RUSTFLAGS"] == expected_flags, "unexpected-rustflags")
    require("MACOSX_DEPLOYMENT_TARGET" not in env or env["MACOSX_DEPLOYMENT_TARGET"] == "14.0", "unexpected-macos-deployment-target")
    # Config files can select replacement sources or inject flags outside the receipt.
    for parent in (source, *source.parents):
        for name in ("config", "config.toml"):
            require(not (parent / ".cargo" / name).exists(), "unreviewed-cargo-config")
    for name in ("config", "config.toml"):
        require(not (cargo_home / name).exists(), "unreviewed-cargo-config")
    return home, cargo_home


def check_source(source, expected):
    require(expected["repository"] in CANONICAL_SOURCE_URLS, "noncanonical-source-repository")
    require(git(source, "rev-parse", "--show-toplevel") == str(source), "source-not-repository-root")
    require(git(source, "rev-parse", "--is-shallow-repository") == "false", "shallow-source")
    require(git(source, "rev-parse", "HEAD") == expected["commit"], "source-commit-mismatch")
    require(git(source, "rev-parse", "HEAD^{tree}") == expected["tree"], "source-tree-mismatch")
    require(git(source, "rev-list", "--max-parents=0", "HEAD").splitlines() == [expected["verified_fresh_history_root"]], "source-root-mismatch")
    require(git(source, "remote").splitlines() == ["origin"], "unexpected-source-remote")
    for args in (("remote", "get-url", "--all", "origin"), ("remote", "get-url", "--push", "--all", "origin")):
        # Require one exact canonical URL and Git's single LF terminator.
        # Neither whitespace stripping nor splitlines() may erase URL bytes.
        output = git(source, *args, strip_output=False)
        require(output in {url + "\n" for url in CANONICAL_SOURCE_URLS}, "noncanonical-source-url")
    require(not git(source, "status", "--porcelain", "--untracked-files=no", allow_empty=True), "modified-source")
    # Hidden modifications must not be omitted by assume-unchanged/skip-worktree.
    tracked = git(source, "ls-files", "-v").splitlines()
    require(all(line.startswith("H ") for line in tracked), "hidden-source-state")
    require(sha256(safe_file(source, "Cargo.lock").read_bytes()) == expected["cargo_lock_sha256"], "source-lock-mismatch")
    return {"commit": expected["commit"], "tree": expected["tree"], "cargo_lock_sha256": expected["cargo_lock_sha256"]}


def parse_tree(text):
    packages = set()
    for line in text.splitlines():
        match = re.fullmatch(r"([A-Za-z0-9_-]+) v([^\s]+)(?: \([^\r\n]+\))?(?: \(\*\))?", line)
        require(match is not None, "unrecognized-package-tree")
        packages.add((match[1], match[2]))
    require(bool(packages), "empty-package-tree")
    return packages


def verify_packages(source, cargo_home, graphs):
    lock = tomllib.loads(safe_file(source, "Cargo.lock").read_text())
    locked = {}
    for p in lock["package"]:
        key = (p["name"], p["version"])
        require(key not in locked, "ambiguous-lock-package")
        locked[key] = p
    verified = []
    for graph in graphs:
        target = graph["target"]
        args = ["cargo", "tree", "--locked", "--offline", "--target", target, "-p", graph["root_package"], "--edges", "normal,build", "--prefix", "none", "--format", "{p}"]
        if graph["features"]:
            args += ["--features", ",".join(graph["features"])]
        actual = parse_tree(run(args, source))
        expected = {(p["name"], p["version"]) for p in graph["packages"]}
        require(len(expected) == graph["package_count"], "invalid-package-contract")
        require(actual == expected, "package-set-mismatch")
        require(not ({"rusb", "libusb1-sys"} & {p[0] for p in actual}), "forbidden-package")
        args = ["cargo", "metadata", "--locked", "--offline", "--format-version", "1", "--filter-platform", target]
        if graph["features"]:
            args += ["--features", ",".join(graph["features"])]
        metadata = json.loads(run(args, source))
        resolved = {}
        for p in metadata["packages"]:
            key = (p["name"], p["version"])
            require(key not in resolved, "ambiguous-resolved-package")
            resolved[key] = p
        for p in graph["packages"]:
            key = (p["name"], p["version"])
            require(key in locked and key in resolved, "package-missing")
            lp, rp = locked[key], resolved[key]
            require(lp.get("source") == p["cargo_source"] == rp.get("source"), "package-source-mismatch")
            require(lp.get("checksum") == p["cargo_checksum"], "package-checksum-mismatch")
            if p["source"]["kind"] == "workspace":
                require(Path(rp["manifest_path"]).resolve() == source / p["source"]["path"] / "Cargo.toml", "workspace-package-path-mismatch")
            else:
                require(p["source"]["kind"] == "registry", "unsupported-package-source")
                manifest = Path(rp["manifest_path"]).resolve()
                registry_src = (cargo_home / "registry/src").resolve()
                require(manifest.is_relative_to(registry_src), "registry-source-path-mismatch")
                relative = manifest.relative_to(registry_src)
                require(len(relative.parts) == 3 and re.fullmatch(r"index\.crates\.io-[a-f0-9]+", relative.parts[0]) and relative.parts[1:] == (f"{p['name']}-{p['version']}", "Cargo.toml"), "registry-source-path-mismatch")
                archive = safe_file(cargo_home / "registry/cache", f"{relative.parts[0]}/{p['name']}-{p['version']}.crate")
                require(sha256(archive.read_bytes()) == p["cargo_checksum"] == p["source"]["archive_sha256"], "registry-archive-checksum-mismatch")
        projection = [{k: p[k] for k in ("name", "version", "cargo_source", "cargo_checksum")} for p in graph["packages"]]
        verified.append({"target": target, "package_count": len(actual), "package_projection_sha256": sha256(json.dumps(projection, sort_keys=True, separators=(",", ":")).encode())})
    return verified


def check_toolchain(ndk, expected, profile=HISTORICAL):
    require(sys.platform == "darwin" and platform.machine() == "arm64", "wrong-host-platform")
    identities = {}
    for key, args in (("rustc_version", ["rustc", "--version"]), ("cargo_version", ["cargo", "--version"]), ("cargo_ndk_version", ["cargo", "ndk", "--version"])):
        value = run(args).strip()
        require(value == expected[key], "toolchain-version-mismatch")
        identities[key] = value
    require("host: aarch64-apple-darwin" in run(["rustc", "-vV"]).splitlines(), "rust-host-mismatch")
    # Tool symlinks inside the NDK are normal; their resolved bytes are pinned.
    for item in expected["ndk_files"]:
        path = ndk / item["path"]
        require(path.is_file() and path.resolve().is_relative_to(ndk), "ndk-file-unavailable")
        require(sha256(path.read_bytes()) == item["sha256"], "ndk-file-hash-mismatch")
    props = (ndk / "source.properties").read_text()
    require(re.search(r"^Pkg\.Revision\s*=\s*" + re.escape(expected["android_ndk_revision"]) + r"\s*$", props, re.M), "ndk-revision-mismatch")
    identities["android_ndk_revision"] = expected["android_ndk_revision"]
    # Apple SDK is recorded, not silently treated as an immutable historical pin.
    for key, args in (("macos_version", ["/usr/bin/sw_vers", "-productVersion"]), ("sdk_version", ["/usr/bin/xcrun", "--sdk", "macosx", "--show-sdk-version"])):
        value = run(args).strip()
        require(re.fullmatch(r"[0-9]+(?:\.[0-9]+){1,2}", value), "unrecognized-apple-version")
        identities[key] = value
    xcode = run(["/usr/bin/xcodebuild", "-version"]).strip()
    require(re.fullmatch(r"Xcode [0-9]+(?:\.[0-9]+){0,2}\nBuild version [A-Za-z0-9]+", xcode), "unrecognized-xcode-version")
    if profile.xcode_identity is not None:
        require(xcode == profile.xcode_identity, "candidate-xcode-identity-mismatch")
    identities["xcode_version"] = xcode
    return identities


def build_path_roles(source, ndk, profile=HISTORICAL):
    home = Path(os.environ["HOME"]).resolve()
    runner_temp = Path(os.environ["RUNNER_TEMP"]).resolve()
    cargo_ndk = shutil.which("cargo-ndk")
    require(cargo_ndk is not None, "cargo-ndk-executable-missing")
    cargo_ndk = Path(cargo_ndk).resolve()
    tool_root = runner_temp / "sondefox-reproduction-tools"
    require(cargo_ndk == tool_root / "bin/cargo-ndk", "cargo-ndk-executable-path-mismatch")
    developer = Path(os.environ["DEVELOPER_DIR"]).resolve()
    if profile == HISTORICAL:
        require(str(developer) == profile.developer_directory, "unreviewed-apple-tool-selection")
    else:
        require(os.environ["DEVELOPER_DIR"] == profile.developer_directory and
                developer == Path(profile.developer_directory).resolve(), "unreviewed-apple-tool-selection")
    return {"$SOURCE": source, "$HOME": home, "$RUSTUP_HOME": runner_temp / "sondefox-reproduction-rustup",
            "$TOOL_ROOT": tool_root, "$NDK_HOME": ndk, "$SCRATCH": runner_temp / "sondefox-reproduction-scratch",
            "$RUNNER_TEMP": runner_temp, "$DEVELOPER_DIR": developer,
            "$ANDROID_SDK_ROOT": Path(os.environ["ANDROID_SDK_ROOT"]),
            "$TMPDIR": Path(os.environ["TMPDIR"])}


def normalized_paths(value, roles):
    aliases = {(role, str(path)) for role, path in roles.items()}
    aliases.update((role, str(path.resolve())) for role, path in roles.items())
    for role, path in sorted(aliases, key=lambda item: len(item[1]), reverse=True):
        value = value.replace(path, role)
    return value


def observed_apple_tools(roles):
    observations = {}
    for tool in ("clang", "nm"):
        selected = run(["/usr/bin/xcrun", "--find", tool]).strip()
        path = Path(selected)
        require(path.is_absolute() and path.is_file() and path.resolve().is_relative_to(roles["$DEVELOPER_DIR"]),
                "apple-tool-path-mismatch")
        version = run([selected, "--version"])
        require(len(version) <= 8192 and (re.search(r"Apple clang version [0-9]+\.[0-9]+", version) if tool == "clang"
                                        else re.search(r"Apple LLVM version [0-9]+\.[0-9]+", version)),
                "unrecognized-apple-tool-version")
        observations["apple_" + tool] = {"path_role": "apple-" + tool, "path": normalized_paths(selected, roles), "sha256": sha256(path.read_bytes()),
                              "version": normalized_paths(version.strip(), roles)}
    darwin = {name: run(["/usr/bin/uname", flag]).strip() for name, flag in
              (("system", "-s"), ("release", "-r"), ("version", "-v"), ("machine", "-m"))}
    require(darwin["system"] == "Darwin" and darwin["machine"] == "arm64"
            and re.fullmatch(r"[0-9]+\.[0-9]+\.[0-9]+", darwin["release"])
            and darwin["version"].startswith("Darwin Kernel Version " + darwin["release"] + ":")
            and len(darwin["version"]) <= 2048, "unrecognized-darwin-identity")
    observations["darwin"] = darwin
    return observations


ANDROID_TARGETS = {"arm64-v8a": "aarch64-linux-android", "x86_64": "x86_64-linux-android"}


def one_option(argv, option):
    values = []
    for index, item in enumerate(argv):
        if item == option:
            require(index + 1 < len(argv), "incomplete-measured-command-option")
            values.append(argv[index + 1])
        elif item.startswith(option + "="):
            values.append(item[len(option) + 1:])
    require(len(values) == 1, "missing-or-duplicate-measured-command-option")
    return values[0]


def jni_crate_types(argv):
    """Cargo may repeat --crate-type or combine the pinned manifest's two kinds."""
    kinds = []
    for index, item in enumerate(argv):
        if item == "--crate-type":
            require(index + 1 < len(argv), "incomplete-measured-command-option")
            kinds.extend(argv[index + 1].split(","))
        elif item.startswith("--crate-type="):
            kinds.extend(item.split("=", 1)[1].split(","))
    require(len(kinds) == 2 and set(kinds) == {"cdylib", "rlib"},
            "android-rustc-crate-types-mismatch")
    return frozenset(kinds)


def observed_assignment(section, key):
    pattern = r'(?<![A-Za-z0-9_])"?' + re.escape(key) + r'"?\s*(?:=|:)\s*("(?:\\.|[^"\\])*"|[^\s,}]+)'
    found = []
    for match in re.finditer(pattern, section):
        token = match[1]
        found.append(json.loads(token) if token.startswith('"') else token)
    require(len(found) == 1, "missing-or-contradictory-android-link-environment")
    return found[0]


def parse_android_build(log, roles):
    """Independently read cargo-ndk's actual verbose environment and JNI command."""
    text = re.sub(r"\x1b\[[0-9;]*m", "", log)
    markers = list(re.finditer(r"(?m)^\s*Building (arm64-v8a|x86_64) \(([^)]+)\)\s*$", text))
    require(len(markers) == 2 and {m[1] for m in markers} == set(ANDROID_TARGETS), "android-build-sections-missing-or-duplicate")
    result = []
    for index, marker in enumerate(markers):
        abi, target = marker[1], ANDROID_TARGETS[marker[1]]
        require(marker[2] == target, "android-section-target-mismatch")
        section = text[marker.end():markers[index + 1].start() if index + 1 < len(markers) else len(text)]
        clang = normalized_paths(observed_assignment(section, "_CARGO_NDK_LINK_CLANG"), roles)
        link_target = observed_assignment(section, "_CARGO_NDK_LINK_TARGET")
        linker = normalized_paths(observed_assignment(section, "CARGO_TARGET_" + target.upper().replace("-", "_") + "_LINKER"), roles)
        require(clang == "$NDK_HOME/toolchains/llvm/prebuilt/darwin-x86_64/bin/clang"
                and linker == "$TOOL_ROOT/bin/cargo-ndk" and link_target == "--target=" + target + "21",
                "android-observed-linker-or-target-mismatch")
        require(observed_assignment(section, "ANDROID_PLATFORM") == "21"
                and observed_assignment(section, "ANDROID_ABI") == abi, "android-observed-api-or-abi-mismatch")
        commands = []
        for match in re.finditer(r"(?m)^\s*Running `([^\n]+)`\s*$", section):
            tokens = shlex.split(match[1])
            if "--crate-name" not in tokens or one_option(tokens, "--crate-name") != "sdr_fox_jni":
                continue
            starts = [i for i, token in enumerate(tokens) if token.startswith("/") and Path(token).name == "rustc"]
            require(len(starts) == 1, "android-rustc-executable-ambiguous")
            argv = [normalized_paths(token, roles) for token in tokens[starts[0]:]]
            require(one_option(argv, "--target") == target, "android-rustc-target-or-kind-mismatch")
            jni_crate_types(argv)
            linker_options = [argv[i + 1][len("linker="):] for i, token in enumerate(argv[:-1])
                              if token == "-C" and argv[i + 1].startswith("linker=")]
            linker_options += [token[len("-Clinker="):] for token in argv if token.startswith("-Clinker=")]
            require(linker_options == [linker], "android-rustc-linker-mismatch")
            require(argv[0] == "$RUSTUP_HOME/toolchains/1.95.0-aarch64-apple-darwin/bin/rustc", "android-rustc-path-mismatch")
            commands.append(argv)
        require(len(commands) == 1, "android-jni-rustc-command-missing-or-duplicate")
        result.append({"abi": abi, "rust_target": target, "api_level": 21, "clang_target": link_target,
                       "clang_path": clang, "linker_path": linker, "rustc_argv": commands[0],
                       "wrapper_assignments": {"ANDROID_PLATFORM": "21", "ANDROID_ABI": abi,
                           "_CARGO_NDK_LINK_CLANG": clang, "_CARGO_NDK_LINK_TARGET": link_target,
                           "CARGO_TARGET_" + target.upper().replace("-", "_") + "_LINKER": linker}})
    return sorted(result, key=lambda row: row["abi"])


def read_build_receipt(path, artifact_root, expected):
    """Read measured build evidence; a receipt alone never proves freshness."""
    require(path.name == "build-receipt.json" and path.parent == artifact_root.parent,
            "build-receipt-location-mismatch")
    raw = safe_file(path.parent, path.name).read_bytes()
    require(len(raw) <= 2 * 1024 * 1024, "build-receipt-too-large")
    def unique_object(pairs):
        result = {}
        for key, value in pairs:
            require(key not in result, "duplicate-build-receipt-key")
            result[key] = value
        return result
    receipt = json.loads(raw, object_pairs_hook=unique_object,
                         parse_constant=lambda _: require(False, "invalid-build-receipt-number"))
    require(type(receipt) is dict and type(receipt.get("schema")) is int and receipt["schema"] == 1
            and receipt.get("status") == "built_pending_independent_verification", "build-not-successful")
    for name, key in (("source_commit", "commit"), ("source_tree", "tree"),
                      ("source_root", "verified_fresh_history_root"), ("lock_sha256", "cargo_lock_sha256")):
        require(receipt.get(name) == expected["source"][key], "build-source-identity-mismatch")
    artifacts = receipt.get("artifacts")
    require(type(artifacts) is list and len(artifacts) == len(expected["artifacts"]), "build-artifact-set-mismatch")
    recorded = {}
    for artifact in artifacts:
        require(type(artifact) is dict and type(artifact.get("path")) is str and artifact["path"] not in recorded,
                "duplicate-or-invalid-build-artifact")
        require(type(artifact.get("size_bytes")) is int, "invalid-build-artifact-size")
        recorded[artifact["path"]] = (artifact.get("sha256"), artifact["size_bytes"])
    wanted = {"artifacts/" + a["path"]: (a["sha256"], a["size_bytes"]) for a in expected["artifacts"]}
    require(recorded == wanted, "build-artifact-identities-mismatch")
    commands = receipt.get("commands")
    require(type(commands) is list and bool(commands), "build-commands-missing")
    for command in commands:
        require(type(command) is dict and command.get("status") == "pass"
                and type(command.get("exit_code")) is int and command["exit_code"] == 0,
                "build-command-unsuccessful")
    return receipt, sha256(raw)


def checked_command(receipt, evidence_root, label):
    matches = [c for c in receipt["commands"] if c.get("label") == label]
    require(len(matches) == 1, "measured-command-missing-or-ambiguous")
    command = matches[0]
    require(command.get("status") == "pass" and type(command.get("exit_code")) is int
            and command["exit_code"] == 0, "measured-command-failed")
    log = command.get("log")
    require(type(log) is str and re.fullmatch(r"logs/[0-9]{2,3}-" + re.escape(label) + r"\.log", log),
            "measured-command-log-path-mismatch")
    data = safe_file(evidence_root, log).read_bytes()
    require(len(data) <= 16 * 1024 * 1024, "measured-command-log-too-large")
    require(sha256(data) == command.get("log_sha256"), "measured-command-log-hash-mismatch")
    argv = command.get("argv")
    require(type(argv) is list and bool(argv) and all(type(arg) is str and 0 < len(arg) <= 8192 for arg in argv),
            "measured-command-argv-missing")
    return argv, data.decode("utf-8")


def referenced_command(receipt, evidence_root, label, reference):
    argv, text = checked_command(receipt, evidence_root, label)
    record = next(c for c in receipt["commands"] if c.get("label") == label)
    require(reference == {"path": record["log"], "sha256": record["log_sha256"]}, "measured-log-reference-mismatch")
    return argv, text


def verify_observed_tools(receipt, evidence_root, roles, actual):
    observed = receipt.get("observed_tools")
    require(type(observed) is dict and set(observed) == {"apple_clang", "apple_nm", "darwin"}, "observed-tool-identities-missing")
    for name in ("clang", "nm"):
        key = "apple_" + name
        item = observed[key]
        require(type(item) is dict and all(item.get(k) == value for k, value in actual[key].items()),
                "observed-apple-tool-identity-mismatch")
        argv, text = referenced_command(receipt, evidence_root, "apple-" + name + "-path", item.get("path_log"))
        require(argv == ["xcrun", "--find", name] and normalized_paths(text.strip(), roles) == item["path"], "observed-tool-path-log-mismatch")
        argv, text = referenced_command(receipt, evidence_root, "apple-" + name + "-version", item.get("version_log"))
        require(argv == [item["path"], "--version"] and normalized_paths(text.strip(), roles) == item["version"], "observed-tool-version-log-mismatch")
    kernel = observed["darwin"]
    require(type(kernel) is dict and all(kernel.get(k) == value for k, value in actual["darwin"].items())
            and type(kernel.get("logs")) is dict and set(kernel["logs"]) == set(actual["darwin"]), "observed-darwin-identity-mismatch")
    for name, flag in (("system", "-s"), ("release", "-r"), ("version", "-v"), ("machine", "-m")):
        argv, text = referenced_command(receipt, evidence_root, "darwin-" + name, kernel["logs"][name])
        require(argv == ["uname", flag] and text.strip() == kernel[name], "observed-darwin-log-mismatch")
    cargo_ndk = safe_file(roles["$TOOL_ROOT"], "bin/cargo-ndk")
    require(sha256(cargo_ndk.read_bytes()) == receipt.get("cargo_ndk_executable_sha256"), "observed-cargo-ndk-executable-mismatch")


def verify_c_smoke(receipt, evidence_root, artifact_root, roles, expected, apple):
    smoke = receipt.get("c_link_smoke")
    require(type(smoke) is dict and smoke.get("status") == "pass", "c-link-smoke-missing-or-failed")
    version = expected["build_evidence"]["c_version"]
    require(smoke.get("expected_version") == smoke.get("version") == version, "c-link-smoke-version-mismatch")
    require(all(type(smoke.get(key)) is int and smoke[key] == 0 for key in ("link_exit_code", "run_exit_code")), "c-link-smoke-command-failed")
    require(smoke.get("source_path") == "$SCRATCH/sdr-version-smoke.c"
            and smoke.get("executable_path") == "$SCRATCH/sdr-version-smoke", "c-link-smoke-path-mismatch")
    source = safe_file(roles["$SCRATCH"], "sdr-version-smoke.c")
    executable = safe_file(roles["$SCRATCH"], "sdr-version-smoke")
    require(sha256(source.read_bytes()) == smoke.get("source_sha256") == expected["build_evidence"]["c_smoke_source_sha256"], "c-link-smoke-source-mismatch")
    identities = {a["role"]: a for a in expected["artifacts"]}
    for role, key in (("c_header", "header_sha256"), ("macos_archive", "archive_sha256")):
        artifact = identities[role]
        require(sha256(safe_file(artifact_root, artifact["path"]).read_bytes()) == smoke.get(key) == artifact["sha256"], "c-link-smoke-input-mismatch")
    executable_hash = sha256(executable.read_bytes())
    require(executable_hash == smoke.get("executable_sha256") and os.access(executable, os.X_OK), "c-link-smoke-executable-mismatch")
    sdk_argv, sdk_log = referenced_command(receipt, evidence_root, "apple-sdk-path", smoke.get("sdk_path_log"))
    sdk_path = run(["/usr/bin/xcrun", "--sdk", "macosx", "--show-sdk-path"]).strip()
    require(Path(sdk_path).is_dir() and Path(sdk_path).resolve().is_relative_to(roles["$DEVELOPER_DIR"])
            and sdk_argv == ["xcrun", "--sdk", "macosx", "--show-sdk-path"]
            and normalized_paths(sdk_log.strip(), roles) == smoke.get("sdk_path") == normalized_paths(sdk_path, roles),
            "c-link-smoke-sdk-path-mismatch")
    argv, _ = referenced_command(receipt, evidence_root, "macos-c-link-smoke", smoke.get("link_log"))
    artifact_path = normalized_paths(str(artifact_root), roles)
    require(argv == [apple["apple_clang"]["path"], "-std=c11", "-Wall", "-Wextra", "-Werror", "-arch", "arm64",
                     "-mmacosx-version-min=14.0", "-isysroot", smoke["sdk_path"], "-I", artifact_path, smoke["source_path"], artifact_path + "/libsdr_fox_ffi.a",
                     "-framework", "IOKit", "-framework", "CoreFoundation", "-liconv", "-lSystem", "-o", smoke["executable_path"]],
            "c-link-smoke-link-command-mismatch")
    argv, output = referenced_command(receipt, evidence_root, "macos-c-run-smoke", smoke.get("run_log"))
    require(argv == [smoke["executable_path"]] and output == version + "\n"
            and sha256(output.encode()) == smoke.get("stdout_sha256"), "c-link-smoke-output-mismatch")
    # Replay only the hash-checked scratch program bound to the fixed C source,
    # generated header, exact archive and link command. Never execute receipt argv.
    require(run([str(executable)], timeout=15) == version + "\n", "c-link-smoke-independent-run-mismatch")
    return {"version": version, "executable_sha256": executable_hash, "independent_run": "pass"}


def parse_clang_driver(log, target, roles):
    commands = []
    for line in log.splitlines():
        if line.lstrip().startswith('"'):
            commands.append([normalized_paths(arg, roles) for arg in shlex.split(line.strip())])
    frontend = [args for args in commands if "-cc1" in args]
    linker = [args for args in commands if args and Path(args[0]).name in ("ld", "ld.lld")]
    require(len(frontend) == len(linker) == 1, "android-clang-commands-missing-or-duplicate")
    triple = one_option(frontend[0], "-triple")
    require(triple == target.replace("-linux-", "-unknown-linux-") + "21", "android-clang-api-triple-mismatch")
    prefix = "$NDK_HOME/toolchains/llvm/prebuilt/darwin-x86_64"
    def actual_path(value):
        require(value.startswith("$NDK_HOME/"), "android-clang-path-outside-ndk")
        path = (roles["$NDK_HOME"] / value[len("$NDK_HOME/"):]).resolve()
        require(path.is_relative_to(roles["$NDK_HOME"].resolve()), "android-clang-path-outside-ndk")
        return path
    binary_dir = roles["$NDK_HOME"] / "toolchains/llvm/prebuilt/darwin-x86_64/bin"
    require(actual_path(frontend[0][0]) == (binary_dir / "clang").resolve()
            and actual_path(linker[0][0]) == (binary_dir / "ld.lld").resolve(), "android-clang-effective-tools-mismatch")
    require("-shared" in linker[0], "android-clang-not-shared-link")
    library = prefix + "/sysroot/usr/lib/" + target + "/21"
    crt = [normalized_paths(str(actual_path(arg)), roles) for arg in linker[0] if Path(arg).name.startswith(("crtbegin", "crtend"))]
    require(crt == [library + "/crtbegin_so.o", library + "/crtend_so.o"], "android-clang-crt-api-mismatch")
    api_paths = [normalized_paths(str(actual_path(arg[2:])), roles) for arg in linker[0] if arg.startswith("-L") and re.search(r"/sysroot/usr/lib/[^/]+/[0-9]+$", arg[2:])]
    require(api_paths == [library], "android-clang-library-api-mismatch")
    return {"cc1_triple": triple, "api_level": 21, "platform_library_path": library, "crt_objects": crt, "linker_argv": linker[0]}


def verify_android_observations(receipt, evidence_root, roles, expected, profile=HISTORICAL):
    rows = receipt.get("android_effective_commands")
    require(type(rows) is list and len(rows) == 2 and all(type(row) is dict for row in rows), "android-observations-missing")
    argv, log = checked_command(receipt, evidence_root, "build-android")
    require(argv == ["cargo", "ndk", "--platform", "21", "-t", "arm64-v8a", "-t", "x86_64", "build", *build_override_args(profile), "-vv", "--color", "never",
                     "--locked", "--offline", "--release", "-p", "sdr-fox-jni", "--features", "android"], "android-build-command-mismatch")
    measured = parse_android_build(log, roles)
    require({row.get("abi") for row in rows} == set(ANDROID_TARGETS), "android-observations-duplicate-or-unknown")
    probe = safe_file(roles["$SCRATCH"], "android-driver.c")
    require(sha256(probe.read_bytes()) == expected["build_evidence"]["android_probe_source_sha256"], "android-probe-source-mismatch")
    for actual in measured:
        row = next(r for r in rows if r["abi"] == actual["abi"])
        if profile.cargo_build_override:
            verify_target_strip(actual["rustc_argv"])
        if profile.android_link_args:
            page_args = check_link_args(actual["rustc_argv"], profile.android_link_args)
            require(row.get("page_link_args") == page_args, "android-page-link-policy-mismatch")
        require(all(row.get(key) == value for key, value in actual.items()), "android-observation-contradicts-build-log")
        referenced_command(receipt, evidence_root, "build-android", row.get("build_log"))
        driver = row.get("clang_driver")
        require(type(driver) is dict, "android-clang-driver-observation-missing")
        label = "android-clang-" + row["rust_target"]
        argv, log = referenced_command(receipt, evidence_root, label, driver.get("log"))
        require(argv == [row["clang_path"], row["clang_target"], "-###", "-shared", "-fPIC", "-x", "c", "$SCRATCH/android-driver.c", "-o", "$SCRATCH/android-driver-" + row["abi"]],
                "android-clang-probe-command-mismatch")
        observed = parse_clang_driver(log, row["rust_target"], roles)
        require(all(driver.get(key) == value for key, value in observed.items())
                and driver.get("probe_source_sha256") == expected["build_evidence"]["android_probe_source_sha256"],
                "android-clang-observation-contradicts-log")
    return [{"abi": row["abi"], "rust_target": row["rust_target"], "api_level": 21} for row in measured]


def verify_build_evidence(receipt_path, source, artifact_root, ndk, expected, *, manifest=None):
    profile = manifest.profile if manifest else HISTORICAL
    receipt, receipt_hash = read_build_receipt(receipt_path, artifact_root, expected)
    if profile != HISTORICAL:
        require(receipt.get("reconstruction_profile") == profile.name and
                receipt.get("expected_manifest_sha256") == manifest.sha256, "build-profile-or-manifest-mismatch")
        verify_receipt_authority(receipt.get("authority"), receipt.get("runner"))
        xcode_argv, observed_xcode = checked_command(receipt, receipt_path.parent, "xcode")
        require(xcode_argv == ["xcodebuild", "-version"] and observed_xcode.strip() == profile.xcode_identity,
                "build-xcode-identity-mismatch")
    roles = build_path_roles(source, ndk, profile)
    host_policy = None
    if profile.cargo_build_override:
        require(receipt.get("cargo_build_override") == profile.cargo_build_override, "build-host-policy-mismatch")
        mac_argv, mac_log = checked_command(receipt, receipt_path.parent, "build-macos")
        require(mac_argv == ["cargo", "build", *build_override_args(profile), "-vv", "--locked", "--offline", "--release",
                             "--target", "aarch64-apple-darwin", "-p", "sdr-fox-cabi"], "macos-build-command-mismatch")
        host_policy = inspect_macos_build(normalized_paths(mac_log, roles),
                                         "$RUSTUP_HOME/toolchains/1.95.0-aarch64-apple-darwin/bin/rustc")
    apple = observed_apple_tools(roles)
    verify_observed_tools(receipt, receipt_path.parent, roles, apple)
    smoke = verify_c_smoke(receipt, receipt_path.parent, artifact_root, roles, expected, apple)
    android = verify_android_observations(receipt, receipt_path.parent, roles, expected, profile)
    result = {"build_receipt_sha256": receipt_hash, "c_link_smoke": smoke, "android_targets": android,
            "apple_tools": {name: {"sha256": value["sha256"], "version_sha256": sha256(value["version"].encode())}
                            for name, value in apple.items() if name != "darwin"},
            "darwin_release": apple["darwin"]["release"], "darwin_version_sha256": sha256(apple["darwin"]["version"].encode())}
    if host_policy is not None:
        result["host_build_policy"] = host_policy
    return result


def validate_artifact_layout(root, artifacts):
    require(root.is_dir() and not root.is_symlink(), "artifact-directory-missing")
    files = set()
    for path in root.rglob("*"):
        require(not path.is_symlink(), "symlink-artifact")
        if path.is_file():
            files.add(path.relative_to(root).as_posix())
        else:
            require(path.is_dir(), "nonregular-artifact")
    require(files == {a["path"] for a in artifacts}, "artifact-file-set-mismatch")


def strict_ar_members(data):
    """Yield exact BSD/System-V member payloads; thin/long-name tables fail closed."""
    require(type(data) is bytes and data.startswith(b"!<arch>\n"), "invalid-provenance-archive")
    position, count = 8, 0
    while position < len(data):
        header = data[position:position + 60]
        require(len(header) == 60 and header[58:] == b"`\n", "invalid-provenance-member-header")
        for field in (header[16:28], header[28:34], header[34:40], header[48:58]):
            require(re.fullmatch(rb"[0-9]+", field.strip(b" ")) is not None, "invalid-provenance-member-number")
        require(re.fullmatch(rb"[0-7]+", header[40:48].strip(b" ")) is not None, "invalid-provenance-member-mode")
        try:
            size = int(header[48:58].strip())
            name = header[:16].rstrip(b" ").decode("ascii")
        except (ValueError, UnicodeError):
            raise VerificationError("invalid-provenance-member-header") from None
        require(name != "//", "unsupported-provenance-long-name-table")
        start, end = position + 60, position + 60 + size
        require(size >= 0 and end <= len(data), "truncated-provenance-member")
        position = end
        if size % 2:
            require(position < len(data) and data[position:position + 1] == b"\n", "invalid-provenance-member-padding")
            position += 1
        if name.startswith("#1/"):
            require(re.fullmatch(r"#1/[1-9][0-9]*", name) is not None, "invalid-provenance-extended-name")
            length = int(name[3:])
            require(length <= size, "invalid-provenance-extended-name")
            raw_name = data[start:start + length]
            unpadded = raw_name.rstrip(b"\0")
            require(b"\0" not in unpadded and bool(unpadded), "invalid-provenance-extended-name")
            try:
                name = unpadded.decode("ascii")
            except UnicodeError:
                raise VerificationError("invalid-provenance-extended-name") from None
            start += length
        elif name not in AR_SYMBOL_NAMES and name.endswith("/"):
            name = name[:-1]
        require(name in AR_SYMBOL_NAMES or re.fullmatch(r"[A-Za-z0-9_.$+-]+", name) is not None,
                "unsupported-provenance-member-name")
        yield name, data[start:end]
        count += 1
    require(position == len(data) and count > 0, "empty-or-truncated-provenance-archive")


def arm64_object(payload):
    return len(payload) >= 32 and payload[:4] == b"\xcf\xfa\xed\xfe" and struct.unpack_from("<I", payload, 4)[0] == 16777228 and struct.unpack_from("<I", payload, 12)[0] == 1


def load_compiler_builtins_reference():
    selected = shutil.which("rustc")
    require(selected is not None, "compiler-reference-rustc-missing")
    compiler = Path(selected).resolve()
    require(compiler.is_file() and sha256(compiler.read_bytes()) == COMPILER_BUILTINS_RUSTC_SHA256,
            "compiler-reference-rustc-mismatch")
    require(run([str(compiler), "--version"]).strip() == COMPILER_BUILTINS_RUSTC_VERSION,
            "compiler-reference-toolchain-mismatch")
    require("host: aarch64-apple-darwin" in run([str(compiler), "-vV"]).splitlines(), "compiler-reference-target-mismatch")
    raw_root = run([str(compiler), "--print", "sysroot"]).strip()
    require(Path(raw_root).is_absolute(), "compiler-reference-sysroot-mismatch")
    root = Path(raw_root).resolve()
    require(root.is_dir() and root.name == COMPILER_BUILTINS_TOOLCHAIN
            and compiler == (root / "bin/rustc").resolve(), "compiler-reference-sysroot-mismatch")
    if "RUSTUP_HOME" in os.environ:
        require(root == (Path(os.environ["RUSTUP_HOME"]) / "toolchains" / COMPILER_BUILTINS_TOOLCHAIN).resolve(),
                "compiler-reference-isolated-sysroot-mismatch")
    reference = safe_file(root, COMPILER_BUILTINS_RELATIVE_PATH).read_bytes()
    require(len(reference) == COMPILER_BUILTINS_SIZE and sha256(reference) == COMPILER_BUILTINS_SHA256,
            "compiler-reference-bytes-mismatch")
    return reference


def classify_compiler_home_paths(data, home, reference):
    """Classify HOME hits only through complete, pinned upstream object identity."""
    require(type(reference) is bytes and len(reference) == COMPILER_BUILTINS_SIZE
            and sha256(reference) == COMPILER_BUILTINS_SHA256, "compiler-reference-bytes-mismatch")
    needle = str(home).encode()
    require(Path(home).is_absolute() and len(needle) > 1, "invalid-home-for-provenance")
    members = collections.Counter()
    for name, payload in strict_ar_members(reference):
        if name in AR_SYMBOL_NAMES or name == "lib.rmeta":
            continue
        require(name.endswith(".o") and arm64_object(payload), "compiler-reference-object-format-mismatch")
        members[(name, len(payload), sha256(payload))] += 1
    require(bool(members), "compiler-reference-objects-missing")
    matched_hits, matched_members = 0, 0
    used = collections.Counter()
    for name, payload in strict_ar_members(data):
        hits = payload.count(needle)
        if not hits:
            continue
        require(name not in AR_SYMBOL_NAMES and arm64_object(payload), "host-path-outside-native-object")
        identity = (name, len(payload), sha256(payload))
        used[identity] += 1
        require(used[identity] <= members[identity], "unproved-or-extra-host-path-object")
        matched_hits += hits
        matched_members += 1
    # This also rejects HOME in member headers/names/padding or across boundaries.
    require(matched_hits == data.count(needle), "host-path-outside-proved-payload")
    return {"inherited_compiler_path_occurrences": matched_hits, "inherited_compiler_path_members": matched_members,
            "reference_sha256": COMPILER_BUILTINS_SHA256}


def check_artifact(root, artifact, forbidden_paths, *, home=None, compiler_reference=None):
    data = safe_file(root, artifact["path"]).read_bytes()
    observation = {"role": artifact["role"], "sha256": sha256(data), "size_bytes": len(data)}
    require(observation["sha256"] == artifact["sha256"] and len(data) == artifact["size_bytes"], "artifact-bytes-mismatch", observation)
    require(not any(str(p).encode() in data for p in forbidden_paths if len(str(p)) > 1), "host-path-in-artifact")
    if home is not None:
        if artifact["role"] == "macos_archive":
            observation.update(classify_compiler_home_paths(data, home, compiler_reference))
        else:
            require(str(home).encode() not in data, "host-path-in-artifact")
    return observation


def declarations(header, kotlin, abi):
    text = re.sub(r"/\*.*?\*/|//[^\n]*", "", header.decode(), flags=re.S)
    c = sorted(set(re.findall(r"\b(sdrfox_[A-Za-z0-9_]+)\s*\([^;{}]*\)\s*;", text, re.S)))
    require(c == abi["c"]["functions"], "c-declarations-mismatch")
    k = kotlin.decode()
    require(re.search(r"^package com\.sdrfox\s*$", k, re.M), "jni-package-mismatch")
    methods = re.findall(r"\bexternal\s+fun\s+([A-Za-z0-9_]+)\s*\(", k)
    require(len(methods) == len(set(methods)), "jni-overloads-unsupported")
    require(sorted(methods) == abi["jni"]["methods"], "jni-declarations-mismatch")
    return {"c_functions": len(c), "jni_methods": len(methods)}


def inspect_elf(data, path, contract, ndk, *, require_page_layout=False):
    require(len(data) >= 64 and data[:4] == b"\x7fELF", "invalid-elf")
    require(data[4] == contract["elf_class"] and data[5] == contract["elf_data"], "elf-format-mismatch")
    typ, machine = struct.unpack_from("<HH", data, 16)
    require(typ == contract["e_type"] and machine == contract["e_machine"], "elf-architecture-mismatch")
    layout = inspect_layout(data) if require_page_layout else None
    require(not any(word.encode().lower() in data.lower() for word in contract["forbidden_byte_strings_case_insensitive"]), "forbidden-elf-content")
    binary = ndk / "toolchains/llvm/prebuilt/darwin-x86_64/bin"
    inspected = run([str(binary / "llvm-readelf"), "-h", "-d", str(path)])
    require(re.search(r"Class:\s+ELF64\b", inspected) and re.search(r"Type:\s+DYN\b", inspected), "elf-inspector-header-mismatch")
    require(re.search(r"Machine:\s+" + re.escape(contract["machine_description"]) + r"\s*$", inspected, re.M), "elf-inspector-machine-mismatch")
    needed = sorted(re.findall(r"\(NEEDED\)\s+Shared library: \[([^\]]+)\]", inspected))
    require(needed == contract["needed"], "elf-needed-mismatch")
    require(not re.search(r"\((?:RPATH|RUNPATH)\)", inspected), "elf-runtime-path")
    symbols = run([str(binary / "llvm-nm"), "-D", "--defined-only", str(path)])
    exported = []
    for line in symbols.splitlines():
        match = re.fullmatch(r"[0-9a-fA-F]+\s+[A-Za-z]\s+(\S+)", line.strip())
        require(match is not None, "unrecognized-elf-symbol-output")
        exported.append(match[1])
    require(sorted(exported) == contract["defined_dynamic_exports"], "elf-exports-mismatch")
    result = {"architecture": contract["android_abi"], "defined_export_count": len(exported), "needed": needed}
    if layout is not None:
        result["page_layout"] = layout
    return result


def parse_macho_archive(data):
    require(data.startswith(b"!<arch>\n"), "invalid-archive")
    position, count = 8, 0
    minima = collections.Counter()
    while position < len(data):
        header = data[position:position + 60]
        require(len(header) == 60 and header[58:] == b"`\n", "invalid-archive-member")
        try:
            size = int(header[48:58].strip())
            name = header[:16].rstrip(b" ").decode("ascii")
        except (ValueError, UnicodeError):
            raise VerificationError("invalid-archive-member") from None
        require(size >= 0 and position + 60 + size <= len(data), "truncated-archive-member")
        member = data[position + 60:position + 60 + size]
        position += 60 + size + size % 2
        if name.startswith("#1/"):
            try:
                length = int(name[3:])
            except ValueError:
                raise VerificationError("invalid-archive-name") from None
            require(0 < length <= len(member), "invalid-archive-name")
            name = member[:length].rstrip(b"\x00").decode("ascii")
            member = member[length:]
        if name.startswith("__.SYMDEF") or name in ("/", "//"):
            continue
        require(len(member) >= 32 and member[:4] == b"\xcf\xfa\xed\xfe", "non-macho-archive-member")
        _, cpu, _, filetype, commands, command_bytes, _, _ = struct.unpack_from("<8I", member)
        require(cpu == 16777228 and filetype == 1, "macho-member-architecture-mismatch")
        require(32 + command_bytes <= len(member), "truncated-macho-commands")
        offset, versions = 32, []
        for _ in range(commands):
            require(offset + 8 <= 32 + command_bytes, "truncated-macho-command")
            cmd, length = struct.unpack_from("<II", member, offset)
            require(length >= 8 and offset + length <= 32 + command_bytes, "invalid-macho-command")
            require((cmd & 0x7FFFFFFF) not in (0xC, 0x18, 0x1F, 0x20, 0x23), "dynamic-dependency-in-static-archive")
            if cmd == 0x32:
                require(length >= 24, "invalid-build-version")
                plat, minimum = struct.unpack_from("<II", member, offset + 8)
                require(plat == 1 and minimum & 0xFF == 0, "macho-platform-mismatch")
                versions.append(f"{minimum >> 16}.{(minimum >> 8) & 255}")
            offset += length
        require(offset == 32 + command_bytes and len(versions) == 1, "macho-build-version-missing")
        minima.update(versions)
        count += 1
    require(position == len(data) and count > 0, "empty-or-truncated-archive")
    return count, dict(minima)


def inspect_macos(data, path, contract):
    count, minima = parse_macho_archive(data)
    require(count == contract["observed_macho_member_count"], "macho-member-count-mismatch")
    require(minima == contract["minimum_versions"], "macho-minimum-versions-mismatch")
    require(run(["/usr/bin/lipo", "-archs", str(path)]).strip() == contract["architecture"], "lipo-architecture-mismatch")
    symbols = run(["/usr/bin/nm", "--no-llvm-bc", "-g", "-U", str(path)])
    exports = sorted(set(re.findall(r"^[0-9a-fA-F]+\s+[A-Za-z]\s+(_sdrfox_\S+)\s*$", symbols, re.M)))
    require(exports == contract["c_abi_defined_exports"], "macho-c-exports-mismatch")
    loads = run(["/usr/bin/otool", "-l", str(path)])
    require(collections.Counter(re.findall(r"\bminos\s+(\S+)", loads)) == collections.Counter(minima), "otool-minimum-versions-mismatch")
    require(re.findall(r"\bplatform\s+(\S+)", loads) == ["1"] * count, "otool-platform-mismatch")
    require(not re.search(r"\bcmd LC_(?:LOAD|REEXPORT|LAZY_LOAD).*DYLIB\b", loads), "otool-dynamic-dependency")
    return {"architecture": "arm64", "macho_members": count, "minimum_versions": minima, "c_export_count": len(exports), "dynamic_dependencies": []}


class Report:
    def __init__(self, manifest=None):
        self.checks = []
        self.manifest = manifest if manifest is not None else load_manifest()

    def check(self, label, fn):
        try:
            value = fn()
        except VerificationError as exc:
            failure = {"check": label, "status": "fail", "code": str(exc)}
            if exc.observation is not None:
                failure["observation"] = exc.observation
            self.checks.append(failure)
            return None
        except (OSError, ValueError, KeyError, TypeError, struct.error, UnicodeError):
            self.checks.append({"check": label, "status": "fail", "code": "invalid-or-unavailable-input"})
            return None
        self.checks.append({"check": label, "status": "pass", "observation": value})
        return value

    def result(self, expected):
        return {"schema": 1, "evidence_kind": self.manifest.profile.evidence_kind, "fresh_build_proven": False,
                "status": "pass" if self.checks and all(c["status"] == "pass" for c in self.checks) else "fail",
                "expected_manifest_sha256": self.manifest.sha256,
                "source_commit": self.manifest.profile.source_commit, "checks": self.checks}


def verify(source, artifact_root, ndk, expected, build_receipt=None, *, manifest=None):
    manifest = manifest if manifest is not None else load_manifest()
    profile = manifest.profile
    report = Report(manifest)
    if expected != manifest.document:
        report.check("manifest", lambda: require(False, "parsed-manifest-identity-mismatch"))
        return report.result(expected)
    env = report.check("environment", lambda: validate_environment(source, os.environ))
    if env is None:
        return report.result(expected)
    # Environment locations are internal only, never report observations.
    report.checks[-1]["observation"] = {"redirecting_overrides": False, "cargo_home": "default"}
    home, cargo_home = env
    if report.check("source", lambda: check_source(source, expected["source"])) is None:
        return report.result(expected)
    if report.check("toolchain", lambda: check_toolchain(ndk, expected["toolchain"], profile)) is None:
        return report.result(expected)
    report.check("package-graphs", lambda: verify_packages(source, cargo_home, expected["package_graphs"]))
    report.check("artifact-layout", lambda: validate_artifact_layout(artifact_root, expected["artifacts"]))
    reference = report.check("compiler-builtins-reference", load_compiler_builtins_reference)
    if reference is not None:
        report.checks[-1]["observation"] = {"toolchain": COMPILER_BUILTINS_TOOLCHAIN,
                                          "target": "aarch64-apple-darwin", "sha256": COMPILER_BUILTINS_SHA256,
                                          "rustc_sha256": COMPILER_BUILTINS_RUSTC_SHA256,
                                          "reference_path": COMPILER_BUILTINS_RELATIVE_PATH,
                                          "size_bytes": COMPILER_BUILTINS_SIZE}
    forbidden = [source, cargo_home]
    if "RUSTUP_HOME" in os.environ:
        forbidden.append(Path(os.environ["RUSTUP_HOME"]).resolve())
    for artifact in expected["artifacts"]:
        report.check(artifact["role"] + "-bytes", lambda a=artifact: check_artifact(artifact_root, a, forbidden, home=home, compiler_reference=reference))
    def binding_checks():
        for a in expected["artifacts"]:
            if a["role"] in ("c_header", "kotlin_binding"):
                require(safe_file(source, a["upstream_output_path"]).read_bytes() == safe_file(artifact_root, a["path"]).read_bytes(), "binding-source-mismatch")
        return declarations(safe_file(artifact_root, "sdr_fox.h").read_bytes(), safe_file(artifact_root, "SdrFox.kt").read_bytes(), expected["abi"])
    report.check("declarations", binding_checks)
    artifact_by_role = {a["role"]: a for a in expected["artifacts"]}
    for contract in expected["abi"]["elf_contracts"]:
        def elf_check(c=contract):
            p = safe_file(artifact_root, artifact_by_role[c["artifact_role"]]["path"])
            return inspect_elf(p.read_bytes(), p, c, ndk, require_page_layout=bool(profile.android_link_args))
        report.check(contract["artifact_role"] + "-abi", elf_check)
    def mac_check():
        p = safe_file(artifact_root, artifact_by_role["macos_archive"]["path"])
        return inspect_macos(p.read_bytes(), p, expected["abi"]["c"]["macos_archive"])
    report.check("macos-abi", mac_check)
    if build_receipt is None:
        report.check("measured-build-evidence", lambda: require(False, "build-receipt-required"))
    else:
        report.check("measured-build-evidence", lambda: verify_build_evidence(build_receipt, source, artifact_root, ndk, expected, manifest=manifest))
    report.check("source-after-inspection", lambda: check_source(source, expected["source"]))
    return report.result(expected)


def write_report(output, result, source, artifacts):
    require(not output.is_symlink(), "unsafe-report-path")
    require(not output.resolve().is_relative_to(source) and not output.resolve().is_relative_to(artifacts), "report-overwrites-input")
    output.parent.mkdir(parents=True, exist_ok=True)
    fd, temporary = tempfile.mkstemp(prefix=".verification-", dir=output.parent)
    try:
        with os.fdopen(fd, "w") as handle:
            json.dump(result, handle, indent=2, sort_keys=True)
            handle.write("\n")
        os.replace(temporary, output)
    finally:
        if os.path.exists(temporary):
            os.unlink(temporary)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--source-dir", required=True, type=Path)
    parser.add_argument("--artifact-dir", required=True, type=Path)
    parser.add_argument("--ndk-dir", required=True, type=Path)
    parser.add_argument("--output", required=True, type=Path)
    parser.add_argument("--build-receipt", required=True, type=Path)
    parser.add_argument("--profile", choices=[profile.name for profile in PROFILES], default=HISTORICAL.name)
    args = parser.parse_args()
    try:
        manifest = load_manifest(select_profile(args.profile))
        expected = manifest.document
        source, artifacts, ndk = (p.resolve() for p in (args.source_dir, args.artifact_dir, args.ndk_dir))
        require(not args.source_dir.is_symlink() and not args.artifact_dir.is_symlink(), "symlink-input-root")
        require(source != artifacts and not artifacts.is_relative_to(source), "artifact-root-overlaps-source")
        require(not args.build_receipt.is_symlink() and args.output.resolve() != args.build_receipt.resolve(), "unsafe-build-receipt-or-report-path")
        result = verify(source, artifacts, ndk, expected, args.build_receipt.resolve(), manifest=manifest)
        write_report(args.output, result, source, artifacts)
    except (VerificationError, OSError, ValueError, KeyError, TypeError):
        print("Verification failed: invalid inputs or report destination.", file=sys.stderr)
        return 1
    label = "Current-pin" if manifest.profile == HISTORICAL else "Candidate"
    print(f"{label} verification: {result['status']} ({len(result['checks'])} checks); freshness requires independent workflow evidence.")
    return 0 if result["status"] == "pass" else 1


if __name__ == "__main__":
    raise SystemExit(main())
