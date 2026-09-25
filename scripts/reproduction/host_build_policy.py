# SPDX-License-Identifier: MIT OR Apache-2.0
"""Inspect actual rustc commands without trusting a build's policy assertion."""
from pathlib import PurePosixPath
import re
import shlex


def require(condition, message):
    if not condition:
        raise ValueError(message)


def values(argv, option):
    result = []
    for index, token in enumerate(argv):
        if token == option:
            require(index + 1 < len(argv), "Truncated compiler option")
            result.append(argv[index + 1])
        elif token.startswith(option + "="):
            result.append(token[len(option) + 1:])
    return result


def strip_settings(argv):
    codegen = values(argv, "-C") + values(argv, "--codegen")
    codegen += [token[2:] for token in argv if token.startswith("-C") and token != "-C" and not token.startswith("-C=")]
    return [item.split("=", 1)[1] for item in codegen if item.startswith("strip=")]


def verify_target_strip(argv):
    require(strip_settings(argv) == ["symbols"], "Runtime target stripping changed")


def inspect_macos_build(log, expected_compiler):
    text = re.sub(r"\x1b\[[0-9;]*m", "", log)
    macros, scripts, runtime = [], 0, 0
    for match in re.finditer(r"(?m)^\s*Running `([^\n]+)`\s*$", text):
        tokens = shlex.split(match[1])
        starts = [i for i, token in enumerate(tokens)
                  if token.startswith(("/", "$RUSTUP_HOME/")) and PurePosixPath(token).name == "rustc"]
        if not starts:
            require(not values(tokens, "--crate-name"), "Unreviewed effective compiler invocation")
            continue
        require(len(starts) == 1, "Ambiguous compiler invocation")
        argv = tokens[starts[0]:]
        names = values(argv, "--crate-name")
        require(len(names) == 1, "Missing compiler crate identity")
        kinds = {kind for item in values(argv, "--crate-type") for kind in item.split(",")}
        host = "proc-macro" in kinds or names == ["build_script_build"]
        target = names == ["sdr_fox"] and values(argv, "--target") == ["aarch64-apple-darwin"]
        if host or target:
            require(argv[0] == expected_compiler, "Unreviewed effective Mac compiler")
        if host:
            require(not values(argv, "--target"), "Build dependency unexpectedly targets a runtime platform")
            require(strip_settings(argv) in ([], ["none"]), "Host build dependency is stripped")
            if "proc-macro" in kinds:
                macros.append(names[0])
            else:
                scripts += 1
        if target:
            require("staticlib" in kinds, "Mac runtime command does not build an archive")
            verify_target_strip(argv)
            runtime += 1
    require(macros.count("thiserror_impl") == 1 and len(macros) == len(set(macros)) and scripts > 0 and runtime == 1,
            "Missing or repeated host/runtime compiler evidence")
    return {"proc_macro_crates": sorted(macros), "build_script_commands": scripts,
            "runtime_archive_commands": runtime, "host_strip": "none", "runtime_strip": "symbols"}
