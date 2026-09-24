#!/usr/bin/env python3
# SPDX-License-Identifier: MIT OR Apache-2.0
"""One cold, fixed-source native build on the private GitHub-hosted Mac job.

No release, vendoring, signing, billing, or public-upload operation exists here.
The separate verifier owns acceptance of the produced bytes.
"""
from __future__ import annotations

import argparse
import hashlib
import json
import os
from pathlib import Path
import platform
import re
import shlex
import shutil
import signal
import subprocess
import sys
import tempfile
import time

SOURCE_COMMIT = "fb34d8c600725b54c5a950234c892a593c343968"
SOURCE_TREE = "fb5fcf5ce09c8c31bb4004b5861d4533c37b581d"
SOURCE_ROOT = "7bcb45cd2a993240abe7f41dcaeec4a84bc04aeb"
LOCK_SHA256 = "62ab5b79776c322f9776c8c41f8a6707fac6dfcf6a49b791fcb30e10999b4f76"
ORIGINS = {"https://github.com/SondeFox/sdr-fox", "https://github.com/SondeFox/sdr-fox.git"}
RUST_TOOLCHAIN = "1.95.0-aarch64-apple-darwin"
RUSTC_VERSION = "rustc 1.95.0 (59807616e 2026-04-14)"
CARGO_VERSION = "cargo 1.95.0 (f2d3ce0bd 2026-03-21)"
NDK_REVISION = "27.2.12479018"
NDK_HASHES = {
    "source.properties": "70f1c5165e6d997b18782a49cff71da7a3caacd0c4cd811da24c43359fb83cf6",
    "toolchains/llvm/prebuilt/darwin-x86_64/bin/clang": "1aac7c103794087632c35c35b83c6d6c8fff4cd01737e5dfbd1b4297942189e2",
    "toolchains/llvm/prebuilt/darwin-x86_64/bin/ld.lld": "06d8ec9a0183bfb70d8c316c6889d21cf8ddb96ffa6e0e2d04716f7217ce106d",
}
CARGO_NDK_CRATE = "903cc87cda6ab7a2ff82a74065e1c2ae5baa869546b32eb4aacabcc6ed5a670f"
TARGETS = ("aarch64-apple-darwin", "aarch64-linux-android", "x86_64-linux-android")
BUILD_SECONDS = 38 * 60  # Leave time for independent verification and upload.
LOG_LIMIT = 16 * 1024 * 1024
SMOKE_SOURCE = '''#include "sdr_fox.h"
#include <stdio.h>
#include <string.h>
int main(void) {
    const char *version = sdrfox_version();
    if (version == NULL || strcmp(version, "0.1.0") != 0) return 1;
    return puts(version) < 0 ? 2 : 0;
}
'''
ANDROID_PROBE_SOURCE = "int main(void) { return 0; }\n"


class BuildError(RuntimeError):
    pass


def require(condition: bool, message: str) -> None:
    if not condition:
        raise BuildError(message)


def sha256(path: Path) -> str:
    h = hashlib.sha256()
    with path.open("rb") as stream:
        for block in iter(lambda: stream.read(1024 * 1024), b""):
            h.update(block)
    return h.hexdigest()


def guard_host(env: dict[str, str]) -> None:
    expected = {"GITHUB_ACTIONS": "true", "RUNNER_ENVIRONMENT": "github-hosted",
                "RUNNER_OS": "macOS", "RUNNER_ARCH": "ARM64",
                "GITHUB_REPOSITORY": "SondeFox/sdr-fox", "GITHUB_REF": "refs/heads/master",
                "GITHUB_EVENT_NAME": "workflow_dispatch",
                "REPRO_PRIVATE_REPOSITORY": "true"}
    for key, value in expected.items():
        require(env.get(key) == value, f"Requires the dedicated hosted workflow: {key}")
    require(platform.system() == "Darwin" and platform.machine() == "arm64", "Requires arm64 macOS")
    overrides = {"CARGO_HOME", "CARGO_TARGET_DIR", "CARGO_ENCODED_RUSTFLAGS", "RUSTFLAGS",
                 "RUSTC", "RUSTDOC", "RUSTC_WRAPPER", "RUSTC_WORKSPACE_WRAPPER",
                 "RUSTUP_TOOLCHAIN", "SDKROOT", "CC", "CXX", "AR", "LD", "CFLAGS",
                 "CXXFLAGS", "LDFLAGS", "LIBRARY_PATH", "GIT_DIR", "GIT_WORK_TREE"}
    for key in env:
        require(key not in overrides and not key.startswith(("GIT_CONFIG", "CARGO_TARGET_", "CARGO_BUILD_", "DYLD_")),
                f"Unexpected build override: {key}")


def remap_flags(source: Path, home: Path) -> str:
    return (f"--remap-path-prefix={source}=/workspace/sdr-fox "
            f"--remap-path-prefix={home / '.cargo'}=/cargo "
            f"--remap-path-prefix={home}=/home/builder")


def guard_source(source: Path, query) -> None:
    require(query(["git", "remote"]).splitlines() == ["origin"], "Only canonical origin is allowed")
    for args in (["git", "remote", "get-url", "--all", "origin"],
                 ["git", "remote", "get-url", "--push", "--all", "origin"]):
        urls = query(args).splitlines()
        require(len(urls) == 1 and urls[0] in ORIGINS, "Unexpected source origin")
    require(query(["git", "rev-parse", "HEAD"]).strip() == SOURCE_COMMIT, "Source commit mismatch")
    require(query(["git", "rev-parse", "HEAD^{tree}"]).strip() == SOURCE_TREE, "Source tree mismatch")
    require(query(["git", "rev-list", "--max-parents=0", "HEAD"]).splitlines() == [SOURCE_ROOT], "Source root mismatch")
    require(not query(["git", "status", "--porcelain", "--untracked-files=all"]).strip(), "Source checkout is not clean")
    require(sha256(source / "Cargo.lock") == LOCK_SHA256, "Cargo.lock mismatch")
    require(not (source / "target").exists(), "Source already has build outputs")
    for parent in (source, *source.parents):
        for name in ("config", "config.toml"):
            require(not (parent / ".cargo" / name).exists(), "Cargo configuration override is present")


def quarantine_cargo_cache(home: Path, scratch: Path) -> list[str]:
    """Move only default Cargo caches on the guarded disposable runner; keep HOME."""
    cargo_home = home / ".cargo"
    require(not cargo_home.is_symlink(), "Default Cargo home must not be a symlink")
    for name in ("config", "config.toml"):
        require(not (cargo_home / name).exists(), "Default Cargo configuration override")
    scratch.mkdir()
    moved = []
    for name in ("registry", "git", ".global-cache", ".package-cache", ".package-cache-mutate"):
        item = cargo_home / name
        require(not item.is_symlink(), "Cargo cache symlink is not permitted")
        if item.exists():
            shutil.move(str(item), str(scratch / name))
            moved.append(name)
    return moved


def build_regenerating_header(header: Path, compile_source) -> None:
    original = header.read_bytes()
    header.unlink()  # Never permit cbindgen's retained-header fallback as proof.
    compile_source()
    require(header.is_file() and not header.is_symlink() and header.read_bytes() == original,
            "Header was not regenerated exactly")


def validate_darwin(observed):
    require(observed["system"] == "Darwin" and observed["machine"] == "arm64", "Unexpected observed Darwin host")
    require(re.fullmatch(r"\d+\.\d+\.\d+", observed["release"]) is not None, "Missing Darwin release")
    require(observed["version"].startswith("Darwin Kernel Version " + observed["release"] + ":"), "Contradictory Darwin identity")


def validate_apple_version(tool, version):
    if tool == "clang":
        require(version.startswith("Apple clang version "), "Missing Apple clang identity")
    else:
        require("llvm-nm" in version and "Apple LLVM version " in version, "Missing Apple nm identity")


def option_values(argv, name):
    values = []
    for i, token in enumerate(argv):
        if token == name:
            require(i + 1 < len(argv), "Missing compiler option value")
            values.append(argv[i + 1])
        elif token.startswith(name + "="):
            values.append(token[len(name) + 1:])
    return values


def parse_android_build(log, clang, linker, rustc):
    """Read cargo-ndk's actual generated target environment and Cargo rustc argv."""
    sections = {}
    current = None
    for line in log.splitlines():
        marker = re.fullmatch(r"\s*Building (arm64-v8a|x86_64) \(([^)]+)\)\s*", line)
        if marker:
            abi, target = marker.groups()
            require(target not in sections, "Repeated Android build section")
            current = {"abi": abi, "assignments": {}, "rustc_commands": []}
            sections[target] = current
            continue
        if current is None:
            continue
        exported = re.fullmatch(r"\s*Exporting ([A-Za-z0-9_-]+)=(.+)\s*", line)
        if exported:
            key, raw = exported.groups()
            if key in {"ANDROID_PLATFORM", "ANDROID_ABI", "_CARGO_NDK_LINK_CLANG", "_CARGO_NDK_LINK_TARGET"} or key.startswith("CARGO_TARGET_") and key.endswith("_LINKER"):
                value = json.loads(raw) if raw.startswith('"') else raw.strip()
                current["assignments"].setdefault(key, []).append(str(value))
        running = re.fullmatch(r"\s*Running `(.*)`\s*", line)
        if running:
            tokens = shlex.split(running.group(1))
            for index, token in enumerate(tokens):
                if Path(token).is_absolute() and Path(token).name == "rustc":
                    argv = tokens[index:]
                    if option_values(argv, "--crate-name") == ["sdr_fox_jni"]:
                        current["rustc_commands"].append(argv)
                    break
    require(set(sections) == set(TARGETS[1:]), "Missing Android target diagnostics")
    result = []
    for target, abi in ((TARGETS[1], "arm64-v8a"), (TARGETS[2], "x86_64")):
        section = sections[target]
        require(section["abi"] == abi, "Contradictory Android ABI")
        expected = {"ANDROID_PLATFORM": "21", "ANDROID_ABI": abi,
                    "_CARGO_NDK_LINK_CLANG": str(clang), "_CARGO_NDK_LINK_TARGET": "--target=" + target + "21",
                    "CARGO_TARGET_" + target.upper().replace("-", "_") + "_LINKER": str(linker)}
        for key, value in expected.items():
            require(section["assignments"].get(key) == [value], "Missing or contradictory Android linker/API assignment: " + key)
        require(set(section["assignments"]) == set(expected), "Unexpected Android linker assignment")
        require(len(section["rustc_commands"]) == 1, "Missing or repeated JNI compiler invocation")
        argv = section["rustc_commands"][0]
        require(argv[0] == str(rustc) and option_values(argv, "--target") == [target], "Unexpected effective rustc/target")
        crate_types = {item for value in option_values(argv, "--crate-type") for item in value.split(",")}
        require("cdylib" in crate_types, "JNI command does not produce a shared library")
        codegen = []
        for i, token in enumerate(argv):
            if token == "-C":
                require(i + 1 < len(argv), "Missing codegen option")
                codegen.append(argv[i + 1])
            elif token.startswith("-C"):
                codegen.append(token[2:])
        require([x for x in codegen if x.startswith("linker=")] == ["linker=" + str(linker)], "Contradictory rustc linker")
        result.append({"rust_target": target, "abi": abi, "api_level": 21,
                       "clang_target": expected["_CARGO_NDK_LINK_TARGET"], "clang_path": str(clang),
                       "linker_path": str(linker), "rustc_argv": argv, "wrapper_assignments": expected})
    return result


def parse_android_clang(log, target, ndk):
    """Require cc1 and linker selections actually printed by pinned clang -###."""
    commands = [shlex.split(line.strip()) for line in log.splitlines() if line.lstrip().startswith('"')]
    frontends = [argv for argv in commands if "-cc1" in argv]
    linkers = [argv for argv in commands if argv and Path(argv[0]).name in {"ld", "ld.lld"}]
    require(len(frontends) == 1 and len(linkers) == 1, "Missing effective clang compiler/linker diagnostics")
    triples = option_values(frontends[0], "-triple")
    expected_triple = target.replace("-linux-", "-unknown-linux-") + "21"
    require(triples == [expected_triple], "Contradictory effective Android API triple")
    require(re.findall(r"^Target: (.+)$", log, re.M) == [expected_triple], "Missing or contradictory clang target identity")
    tools = ndk / "toolchains/llvm/prebuilt/darwin-x86_64/bin"
    require(Path(frontends[0][0]).resolve() == (tools / "clang").resolve(), "Unexpected effective NDK compiler")
    require(Path(linkers[0][0]).resolve() == (tools / "ld.lld").resolve(), "Unexpected effective NDK linker")
    library = (ndk / "toolchains/llvm/prebuilt/darwin-x86_64/sysroot/usr/lib" / target / "21").resolve()
    require("-shared" in linkers[0], "Android probe did not resolve a shared-library link")
    crt = [str(library / "crtbegin_so.o"), str(library / "crtend_so.o")]
    observed_crt = [str(Path(arg).resolve()) for arg in linkers[0] if Path(arg).name.startswith(("crtbegin", "crtend"))]
    require(observed_crt == crt, "Missing or contradictory Android API startup objects")
    api_paths = [str(Path(arg[2:]).resolve()) for arg in linkers[0] if arg.startswith("-L") and re.search(r"/sysroot/usr/lib/[^/]+/\d+$", arg[2:])]
    require(api_paths == [str(library)], "Missing or contradictory Android API library directory")
    return {"cc1_triple": triples[0], "api_level": 21, "platform_library_path": str(library),
            "crt_objects": crt, "linker_argv": linkers[0]}


class Builder:
    def __init__(self, source: Path, evidence: Path, env: dict[str, str]):
        self.source, self.evidence = source, evidence
        self.original_env = env
        self.deadline = time.monotonic() + BUILD_SECONDS
        self.receipt = {"schema": 1, "status": "running", "source_commit": SOURCE_COMMIT,
                        "source_tree": SOURCE_TREE, "source_root": SOURCE_ROOT,
                        "lock_sha256": LOCK_SHA256, "commands": [], "artifacts": [],
                        "scope": "One hosted reconstruction attempt; acceptance belongs to independent verifier"}
        self.evidence.mkdir(parents=False, exist_ok=False)
        (self.evidence / "logs").mkdir()
        self.env = {key: env[key] for key in ("HOME", "PATH", "TMPDIR", "DEVELOPER_DIR", "ANDROID_SDK_ROOT") if key in env}
        self.env.update({"LANG": "en_US.UTF-8", "LC_ALL": "en_US.UTF-8", "CARGO_TERM_COLOR": "never"})
        self.log_number = 0
        self.roles = {"$SOURCE": str(source)}
        for key in ("HOME", "RUNNER_TEMP", "DEVELOPER_DIR", "ANDROID_SDK_ROOT"):
            if env.get(key):
                self.roles["$" + key] = str(Path(env[key]).resolve())
        self.roles["$TMPDIR"] = env.get("TMPDIR", tempfile.gettempdir()).rstrip("/")

    def sanitized(self, value):
        if isinstance(value, list):
            return [self.sanitized(item) for item in value]
        if isinstance(value, dict):
            return {key: self.sanitized(item) for key, item in value.items()}
        if isinstance(value, str):
            replacements = {(role, actual) for role, actual in self.roles.items()}
            replacements.update((role, str(Path(actual).resolve())) for role, actual in self.roles.items())
            for role, actual in sorted(replacements, key=lambda item: (-len(item[1]), item[0])):
                value = value.replace(actual, role)
        return value

    def log_reference(self, label):
        records = [item for item in self.receipt["commands"] if item["label"] == label]
        require(len(records) == 1 and records[0]["status"] == "pass", "Missing successful command evidence: " + label)
        return {"path": records[0]["log"], "sha256": records[0]["log_sha256"]}

    def save_receipt(self) -> None:
        (self.evidence / "build-receipt.json").write_text(json.dumps(self.receipt, indent=2, sort_keys=True) + "\n")

    def command(self, label: str, argv: list[str], *, env=None, cwd=None, query=False) -> str:
        remaining = self.deadline - time.monotonic()
        require(remaining > 0, "Cold-build time budget exhausted")
        self.log_number += 1
        log = self.evidence / "logs" / f"{self.log_number:02d}-{label}.log"
        record = {"label": label, "argv": self.sanitized(argv), "log": str(log.relative_to(self.evidence)), "status": "running"}
        self.receipt["commands"].append(record)
        self.save_receipt()
        print(f"Reproduction: {label}", flush=True)
        timed_out = False
        with log.open("wb") as stream:
            process = subprocess.Popen(argv, cwd=cwd or self.source, env=env or self.env,
                                       stdout=stream, stderr=subprocess.STDOUT, start_new_session=True)
            try:
                code = process.wait(timeout=min(remaining, 20 * 60))
            except subprocess.TimeoutExpired:
                os.killpg(process.pid, signal.SIGKILL)
                process.wait()
                timed_out = True
                code = process.returncode
        record.update(status="timeout" if timed_out else ("pass" if code == 0 else "failed"),
                      exit_code=code, log_sha256=sha256(log))
        if log.stat().st_size > LOG_LIMIT:
            with log.open("rb") as stream:
                stream.seek(-LOG_LIMIT, os.SEEK_END)
                tail = stream.read()
            log.write_bytes(b"[Log exceeded bound; prefix omitted; build failed]\n" + tail)
            record.update(status="log_limit_exceeded", log_sha256=sha256(log))
        self.save_receipt()
        require(record["status"] == "pass", f"Command failed; inspect {record['log']}")
        return log.read_text() if query else ""

    def copy_artifact(self, source: Path, relative: str) -> None:
        require(source.is_file() and not source.is_symlink(), f"Missing regular output: {relative}")
        output = self.evidence / "artifacts" / relative
        output.parent.mkdir(parents=True, exist_ok=True)
        shutil.copyfile(source, output)
        self.receipt["artifacts"].append({"path": str(output.relative_to(self.evidence)),
                                          "size_bytes": output.stat().st_size, "sha256": sha256(output)})

    def observe_apple_tools(self):
        darwin = {}
        logs = {}
        for key, flag in (("system", "-s"), ("release", "-r"), ("version", "-v"), ("machine", "-m")):
            label = "darwin-" + key
            darwin[key] = self.command(label, ["uname", flag], query=True).strip()
            logs[key] = self.log_reference(label)
        validate_darwin(darwin)
        observed = {"darwin": darwin | {"logs": logs}}
        tools = {}
        for tool in ("clang", "nm"):
            prefix = "apple-" + tool
            path = Path(self.command(prefix + "-path", ["xcrun", "--find", tool], query=True).strip())
            require(path.is_absolute() and path.is_file(), "Missing selected Apple tool")
            version = self.command(prefix + "-version", [str(path), "--version"], query=True).strip()
            validate_apple_version(tool, version)
            observed["apple_" + tool] = {"path_role": prefix, "path": str(path), "sha256": sha256(path),
                "version": version, "path_log": self.log_reference(prefix + "-path"),
                "version_log": self.log_reference(prefix + "-version")}
            tools[tool] = path
        self.receipt["observed_tools"] = self.sanitized(observed)
        self.save_receipt()
        return tools

    def c_link_smoke(self, archive, header, scratch, clang):
        source = scratch / "sdr-version-smoke.c"
        executable = scratch / "sdr-version-smoke"
        source.write_text(SMOKE_SOURCE)
        self.roles["$SCRATCH"] = str(scratch.resolve())
        record = {"status": "running", "expected_version": "0.1.0",
                  "source_path": self.sanitized(str(source)), "source_sha256": sha256(source),
                  "header_sha256": sha256(header), "archive_sha256": sha256(archive),
                  "executable_path": self.sanitized(str(executable))}
        self.receipt["c_link_smoke"] = record
        self.save_receipt()
        try:
            sdk = Path(self.command("apple-sdk-path", ["xcrun", "--sdk", "macosx", "--show-sdk-path"], query=True).strip())
            require(sdk.is_absolute() and sdk.is_dir(), "Missing observed macOS SDK for C smoke")
            record.update(sdk_path=self.sanitized(str(sdk)), sdk_path_log=self.log_reference("apple-sdk-path"))
            self.command("macos-c-link-smoke", [str(clang), "-std=c11", "-Wall", "-Wextra", "-Werror",
                "-arch", "arm64", "-mmacosx-version-min=14.0", "-isysroot", str(sdk), "-I", str(header.parent), str(source), str(archive),
                "-framework", "IOKit", "-framework", "CoreFoundation", "-liconv", "-lSystem", "-o", str(executable)])
            record.update(link_exit_code=0, link_log=self.log_reference("macos-c-link-smoke"), executable_sha256=sha256(executable))
            output = self.command("macos-c-run-smoke", [str(executable)], query=True)
            run_log = self.log_reference("macos-c-run-smoke")
            raw_output = (self.evidence / run_log["path"]).read_bytes()
            record.update(run_exit_code=0, run_log=run_log, stdout_sha256=hashlib.sha256(raw_output).hexdigest())
            require(raw_output == b"0.1.0\n" and output == "0.1.0\n", "C link/run smoke returned an unexpected version")
            record.update(status="pass", version="0.1.0")
        except Exception:
            record["status"] = "failed"
            raise
        finally:
            self.save_receipt()

    def observe_android_commands(self, build_log, ndk, tool_root, rust_bin, scratch):
        clang = ndk / "toolchains/llvm/prebuilt/darwin-x86_64/bin/clang"
        observations = parse_android_build(build_log, clang, tool_root / "bin/cargo-ndk", rust_bin / "rustc")
        source = scratch / "android-driver.c"
        source.write_text(ANDROID_PROBE_SOURCE)
        for row in observations:
            target = row["rust_target"]
            label = "android-clang-" + target
            output = self.command(label, [row["clang_path"], row["clang_target"], "-###", "-shared", "-fPIC", "-x", "c",
                                         str(source), "-o", str(scratch / ("android-driver-" + row["abi"]))], query=True)
            row["build_log"] = self.log_reference("build-android")
            row["clang_driver"] = parse_android_clang(output, target, ndk) | {
                "log": self.log_reference(label), "probe_source_sha256": sha256(source)}
        self.receipt["android_effective_commands"] = self.sanitized(observations)
        self.save_receipt()

    def prefetch_locked_metadata(self):
        # cbindgen's metadata query includes every target and all features, even
        # though the production builds use only Mac and Android. Target-filtered
        # fetching missed already-locked Windows metadata crates on a cold host.
        # Fetch the complete lockfile without compiling those other targets,
        # then prove the same metadata query works offline BEFORE deleting the
        # header. Never weaken the later actual-header-regeneration check.
        self.command("fetch-locked-crates", ["cargo", "fetch", "--locked"])
        self.env["CARGO_NET_OFFLINE"] = "true"
        self.command("preflight-cabi-metadata", ["cargo", "metadata", "--locked", "--offline",
            "--all-features", "--format-version", "1", "--manifest-path",
            str(self.source / "crates/sdr-fox-cabi/Cargo.toml")])

    def run(self) -> None:
        env = self.original_env
        guard_host(env)
        runner_temp = Path(env["RUNNER_TEMP"]).resolve()
        home = Path(env["HOME"]).resolve()
        require(self.source == Path(env["GITHUB_WORKSPACE"]).resolve() / "fixed-source", "Unexpected source checkout path")
        require(self.evidence == runner_temp / "sondefox-reproduction", "Unexpected evidence output path")
        require(self.env.get("DEVELOPER_DIR") == "/Applications/Xcode_26.6.app/Contents/Developer", "Unexpected Xcode selection")
        require(Path(self.env["DEVELOPER_DIR"]).is_dir(), "Required Xcode 26.6 is unavailable")
        self.receipt["runner"] = {key: env.get(key) for key in ("GITHUB_RUN_ID", "GITHUB_RUN_ATTEMPT", "GITHUB_SHA", "RUNNER_OS", "RUNNER_ARCH", "RUNNER_ENVIRONMENT", "ImageOS", "ImageVersion")}
        guard_source(self.source, lambda args: self.command("source-check", args, query=True))
        scratch = runner_temp / "sondefox-reproduction-scratch"
        scratch.mkdir(exist_ok=False)
        rustup_home = runner_temp / "sondefox-reproduction-rustup"
        tool_root = runner_temp / "sondefox-reproduction-tools"
        self.roles.update({"$SCRATCH": str(scratch), "$RUSTUP_HOME": str(rustup_home), "$TOOL_ROOT": str(tool_root)})
        require(not rustup_home.exists() and not tool_root.exists(), "Pinned tool directories must be new")
        self.receipt["cold_boundary"] = {"reused_actions_cache": False, "reused_target": False,
                                        "quarantined_default_cargo_entries": quarantine_cargo_cache(home, scratch / "unused-warm-cargo"),
                                        "bootstrap_cargo_separate": True, "home_unchanged": True}
        self.env["RUSTUP_HOME"] = str(rustup_home)
        for label, args in (("os", ["sw_vers"]), ("xcode", ["xcodebuild", "-version"]),
                            ("sdk", ["xcrun", "--sdk", "macosx", "--show-sdk-version"]),
                            ("rustup", ["rustup", "--version"]),
                            ("python", [sys.executable, "--version"])):
            self.command(label, args)
        apple_tools = self.observe_apple_tools()
        self.receipt["bootstrap_executables"] = {
            tool: sha256(Path(shutil.which(tool, path=self.env["PATH"])))
            for tool in ("rustup", "python3")
        }
        source_archive = scratch / "source.tar"
        self.command("source-archive", ["git", "archive", "--format=tar", "--output", str(source_archive), SOURCE_COMMIT])
        self.receipt["source_archive_sha256"] = sha256(source_archive)
        self.command("install-rust", ["rustup", "toolchain", "install", "1.95.0", "--profile", "minimal",
                                      "--no-self-update", "--target", ",".join(TARGETS)])
        rust_bin = rustup_home / "toolchains" / RUST_TOOLCHAIN / "bin"
        self.env["PATH"] = os.pathsep.join((str(rust_bin), str(tool_root / "bin"), self.env["PATH"]))
        for tool, expected in (("rustc", RUSTC_VERSION), ("cargo", CARGO_VERSION)):
            require(self.command(tool + "-version", [tool, "--version"], query=True).strip() == expected, f"Unexpected {tool} identity")
            self.command(tool + "-verbose-version", [tool, "-vV"])
        inventory = [{"path": str(p.relative_to(rustup_home)), "sha256": sha256(p)}
                     for p in sorted(rustup_home.rglob("*")) if p.is_file()]
        (self.evidence / "rust-toolchain-inventory.json").write_text(json.dumps(inventory, indent=2) + "\n")
        bootstrap = self.env | {"CARGO_HOME": str(scratch / "bootstrap-cargo"), "CARGO_TARGET_DIR": str(scratch / "bootstrap-target")}
        self.command("fetch-cargo-ndk", ["cargo", "info", "cargo-ndk@4.1.2", "--registry", "crates-io"], env=bootstrap, cwd=scratch)
        archives = list((scratch / "bootstrap-cargo" / "registry" / "cache").glob("*/cargo-ndk-4.1.2.crate"))
        require(len(archives) == 1 and sha256(archives[0]) == CARGO_NDK_CRATE, "cargo-ndk source checksum mismatch")
        self.receipt["cargo_ndk_source_sha256"] = sha256(archives[0])
        self.command("install-cargo-ndk", ["cargo", "install", "cargo-ndk", "--version", "=4.1.2", "--locked", "--root", str(tool_root)], env=bootstrap, cwd=scratch)
        require(self.command("cargo-ndk-version", ["cargo", "ndk", "--version"], query=True).strip() == "cargo-ndk 4.1.2", "Unexpected cargo-ndk identity")
        self.receipt["cargo_ndk_executable_sha256"] = sha256(tool_root / "bin" / "cargo-ndk")
        sdk = Path(self.env["ANDROID_SDK_ROOT"]).resolve()
        ndk = sdk / "ndk" / NDK_REVISION
        self.roles["$NDK_HOME"] = str(ndk)
        require(not ndk.exists(), "Expected NDK must be newly installed on this runner")
        sdkmanager = sdk / "cmdline-tools" / "latest" / "bin" / "sdkmanager"
        self.receipt["bootstrap_executables"]["sdkmanager"] = sha256(sdkmanager)
        self.command("sdkmanager-version", [str(sdkmanager), "--version"], cwd=scratch)
        self.command("install-ndk", [str(sdkmanager), f"ndk;{NDK_REVISION}"], cwd=scratch)
        for relative, expected in NDK_HASHES.items():
            require(sha256(ndk / relative) == expected, f"NDK file checksum mismatch: {relative}")
        properties = (ndk / "source.properties").read_text()
        require(re.search(r"^Pkg.Revision\s*=\s*(\S+)\s*$", properties, re.M).group(1) == NDK_REVISION, "NDK revision mismatch")
        self.env["ANDROID_NDK_HOME"] = str(ndk)
        self.env["RUSTFLAGS"] = remap_flags(self.source, home)
        self.receipt["build_environment"] = {"RUSTFLAGS": self.sanitized(self.env["RUSTFLAGS"]), "CARGO_HOME": "unset; default HOME/.cargo",
                                             "ANDROID_NDK_REVISION": NDK_REVISION, "ANDROID_API": 21, "MACOSX_DEPLOYMENT_TARGET": "14.0"}
        # A fresh default registry is separate from cargo-ndk's bootstrap registry.
        require(not (home / ".cargo" / "registry").exists() and not (home / ".cargo" / "git").exists(), "Default production Cargo cache was populated before fetch")
        self.prefetch_locked_metadata()
        graphs = self.evidence / "graphs"
        graphs.mkdir()
        for target in TARGETS:
            mac = target.endswith("apple-darwin")
            args = ["cargo", "tree", "--locked", "--offline", "--target", target, "-p",
                    "sdr-fox-cabi" if mac else "sdr-fox-jni", "--edges", "normal,build", "--prefix", "none", "--format", "{p}"]
            if not mac:
                args += ["--features", "android"]
            graph = self.command("graph-" + target, args, query=True)
            (graphs / (target + ".txt")).write_text(graph)
            require(not re.search(r"(?m)^(rusb|libusb1-sys) v", graph), "Forbidden USB package in target graph")
        header = self.source / "bindings" / "sdr_fox.h"
        before = header.read_bytes()
        self.receipt["header_regeneration"] = {"removed_before_build": True, "original_sha256": hashlib.sha256(before).hexdigest()}
        self.save_receipt()
        build_regenerating_header(header, lambda: self.command(
            "build-macos", ["cargo", "build", "--locked", "--offline", "--release", "--target", TARGETS[0], "-p", "sdr-fox-cabi"],
            env=self.env | {"MACOSX_DEPLOYMENT_TARGET": "14.0"}))
        self.receipt["header_regeneration"]["regenerated_exactly"] = True
        self.copy_artifact(self.source / "target" / TARGETS[0] / "release" / "libsdr_fox.a", "libsdr_fox_ffi.a")
        self.copy_artifact(header, "sdr_fox.h")
        self.copy_artifact(self.source / "bindings" / "android" / "SdrFox.kt", "SdrFox.kt")
        self.c_link_smoke(self.evidence / "artifacts/libsdr_fox_ffi.a", self.evidence / "artifacts/sdr_fox.h", scratch, apple_tools["clang"])
        android_log = self.command("build-android", ["cargo", "ndk", "--platform", "21", "-t", "arm64-v8a", "-t", "x86_64", "build", "-vv", "--color", "never", "--locked", "--offline", "--release", "-p", "sdr-fox-jni", "--features", "android"], query=True)
        self.observe_android_commands(android_log, ndk, tool_root, rust_bin, scratch)
        for abi, target in (("arm64-v8a", TARGETS[1]), ("x86_64", TARGETS[2])):
            self.copy_artifact(self.source / "target" / target / "release" / "libsdr_fox_jni.so", abi + "/libsdr_fox_jni.so")
        require(not self.command("final-source-cleanliness", ["git", "status", "--porcelain", "--untracked-files=no"], query=True).strip(), "Tracked source changed during build")
        self.receipt["status"] = "built_pending_independent_verification"


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--source-dir", type=Path, required=True)
    parser.add_argument("--evidence-dir", type=Path, required=True)
    args = parser.parse_args()
    builder = None
    try:
        guard_host(dict(os.environ))  # Never quarantine local developer caches.
        builder = Builder(args.source_dir.resolve(), args.evidence_dir.resolve(), dict(os.environ))
        builder.run()
        return 0
    except Exception as exc:
        if builder:
            builder.receipt.update(status="failed", error=str(exc))
        print(f"Reproduction failed: {exc}", file=sys.stderr)
        return 1
    finally:
        if builder:
            builder.save_receipt()


if __name__ == "__main__":
    raise SystemExit(main())
