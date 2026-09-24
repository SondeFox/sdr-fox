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
import struct
import subprocess
import sys
import tempfile
import tomllib

EXPECTED_PATH = Path(__file__).with_name("expected.json")


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


def run(argv, cwd=None):
    try:
        result = subprocess.run(argv, cwd=cwd, capture_output=True, text=True,
                                encoding="utf-8", errors="strict", timeout=180)
    except (OSError, subprocess.TimeoutExpired, UnicodeError):
        raise VerificationError("inspection-command-unavailable") from None
    command = Path(argv[0]).name
    command = command if command in {"git", "cargo", "rustc", "lipo", "nm", "otool", "llvm-readelf", "llvm-nm", "sw_vers", "xcrun", "xcodebuild"} else "inspection-tool"
    require(result.returncode == 0, "inspection-command-failed", {"command": command, "exit_code": result.returncode})
    require(bool(result.stdout.strip()), "inspection-command-empty")
    return result.stdout


def git(source, *args, allow_empty=False):
    argv = ["git", "--no-replace-objects", "-c", "core.fsmonitor=false", "-c", "core.hooksPath=/dev/null", "-C", str(source), *args]
    if allow_empty:
        try:
            result = subprocess.run(argv, capture_output=True, text=True, timeout=30)
        except (OSError, subprocess.TimeoutExpired):
            raise VerificationError("source-command-unavailable") from None
        require(result.returncode == 0, "source-command-failed")
        return result.stdout.strip()
    return run(argv).strip()


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
    require(git(source, "rev-parse", "--show-toplevel") == str(source), "source-not-repository-root")
    require(git(source, "rev-parse", "--is-shallow-repository") == "false", "shallow-source")
    require(git(source, "rev-parse", "HEAD") == expected["commit"], "source-commit-mismatch")
    require(git(source, "rev-parse", "HEAD^{tree}") == expected["tree"], "source-tree-mismatch")
    require(git(source, "rev-list", "--max-parents=0", "HEAD").splitlines() == [expected["verified_fresh_history_root"]], "source-root-mismatch")
    require(git(source, "remote").splitlines() == ["origin"], "unexpected-source-remote")
    for args in (("remote", "get-url", "--all", "origin"), ("remote", "get-url", "--push", "--all", "origin")):
        require(git(source, *args).splitlines() == [expected["repository"]], "noncanonical-source-url")
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


def check_toolchain(ndk, expected):
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
    identities["xcode_version"] = xcode
    return identities


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


def check_artifact(root, artifact, forbidden_paths):
    data = safe_file(root, artifact["path"]).read_bytes()
    observation = {"role": artifact["role"], "sha256": sha256(data), "size_bytes": len(data)}
    require(observation["sha256"] == artifact["sha256"] and len(data) == artifact["size_bytes"], "artifact-bytes-mismatch", observation)
    require(not any(str(p).encode() in data for p in forbidden_paths if len(str(p)) > 1), "host-path-in-artifact")
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


def inspect_elf(data, path, contract, ndk):
    require(len(data) >= 64 and data[:4] == b"\x7fELF", "invalid-elf")
    require(data[4] == contract["elf_class"] and data[5] == contract["elf_data"], "elf-format-mismatch")
    typ, machine = struct.unpack_from("<HH", data, 16)
    require(typ == contract["e_type"] and machine == contract["e_machine"], "elf-architecture-mismatch")
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
    return {"architecture": contract["android_abi"], "defined_export_count": len(exported), "needed": needed}


def parse_macho_archive(data):
    require(data.startswith(b"!<arch>\n"), "invalid-archive")
    position, count = 8, 0
    minima = collections.Counter()
    while position < len(data):
        header = data[position:position + 60]
        require(len(header) == 60 and header[58:] == b"`\n", "invalid-archive-member")
        try:
            size = int(header[48:58].strip())
            name = header[:16].rstrip().decode("ascii")
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
    def __init__(self):
        self.checks = []

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
        return {"schema": 1, "evidence_kind": "current-pin-artifact-verification", "fresh_build_proven": False,
                "status": "pass" if self.checks and all(c["status"] == "pass" for c in self.checks) else "fail",
                "expected_manifest_sha256": sha256(EXPECTED_PATH.read_bytes()),
                "source_commit": expected["source"]["commit"], "checks": self.checks}


def verify(source, artifact_root, ndk, expected):
    report = Report()
    env = report.check("environment", lambda: validate_environment(source, os.environ))
    if env is None:
        return report.result(expected)
    # Environment locations are internal only, never report observations.
    report.checks[-1]["observation"] = {"redirecting_overrides": False, "cargo_home": "default"}
    home, cargo_home = env
    if report.check("source", lambda: check_source(source, expected["source"])) is None:
        return report.result(expected)
    if report.check("toolchain", lambda: check_toolchain(ndk, expected["toolchain"])) is None:
        return report.result(expected)
    report.check("package-graphs", lambda: verify_packages(source, cargo_home, expected["package_graphs"]))
    report.check("artifact-layout", lambda: validate_artifact_layout(artifact_root, expected["artifacts"]))
    forbidden = [source, home, cargo_home]
    if "RUSTUP_HOME" in os.environ:
        forbidden.append(Path(os.environ["RUSTUP_HOME"]).resolve())
    for artifact in expected["artifacts"]:
        report.check(artifact["role"] + "-bytes", lambda a=artifact: check_artifact(artifact_root, a, forbidden))
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
            return inspect_elf(p.read_bytes(), p, c, ndk)
        report.check(contract["artifact_role"] + "-abi", elf_check)
    def mac_check():
        p = safe_file(artifact_root, artifact_by_role["macos_archive"]["path"])
        return inspect_macos(p.read_bytes(), p, expected["abi"]["c"]["macos_archive"])
    report.check("macos-abi", mac_check)
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
    args = parser.parse_args()
    try:
        expected = json.loads(EXPECTED_PATH.read_text())
        source, artifacts, ndk = (p.resolve() for p in (args.source_dir, args.artifact_dir, args.ndk_dir))
        require(not args.source_dir.is_symlink() and not args.artifact_dir.is_symlink(), "symlink-input-root")
        require(source != artifacts and not artifacts.is_relative_to(source), "artifact-root-overlaps-source")
        result = verify(source, artifacts, ndk, expected)
        write_report(args.output, result, source, artifacts)
    except (VerificationError, OSError, ValueError, KeyError, TypeError):
        print("Verification failed: invalid inputs or report destination.", file=sys.stderr)
        return 1
    print(f"Current-pin verification: {result['status']} ({len(result['checks'])} checks); freshness requires independent workflow evidence.")
    return 0 if result["status"] == "pass" else 1


if __name__ == "__main__":
    raise SystemExit(main())
