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
import shutil
import signal
import subprocess
import sys
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

    def save_receipt(self) -> None:
        (self.evidence / "build-receipt.json").write_text(json.dumps(self.receipt, indent=2, sort_keys=True) + "\n")

    def command(self, label: str, argv: list[str], *, env=None, cwd=None, query=False) -> str:
        remaining = self.deadline - time.monotonic()
        require(remaining > 0, "Cold-build time budget exhausted")
        self.log_number += 1
        log = self.evidence / "logs" / f"{self.log_number:02d}-{label}.log"
        record = {"label": label, "argv": argv, "log": str(log.relative_to(self.evidence)), "status": "running"}
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
        self.receipt["build_environment"] = {"RUSTFLAGS": self.env["RUSTFLAGS"], "CARGO_HOME": "unset; default HOME/.cargo",
                                             "ANDROID_NDK_REVISION": NDK_REVISION, "ANDROID_API": 21, "MACOSX_DEPLOYMENT_TARGET": "14.0"}
        # A fresh default registry is separate from cargo-ndk's bootstrap registry.
        require(not (home / ".cargo" / "registry").exists() and not (home / ".cargo" / "git").exists(), "Default production Cargo cache was populated before fetch")
        fetch = ["cargo", "fetch", "--locked"]
        for target in TARGETS:
            fetch += ["--target", target]
        self.command("fetch-locked-crates", fetch)
        self.env["CARGO_NET_OFFLINE"] = "true"
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
        self.command("build-android", ["cargo", "ndk", "--platform", "21", "-t", "arm64-v8a", "-t", "x86_64", "build", "--locked", "--offline", "--release", "-p", "sdr-fox-jni", "--features", "android"])
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
