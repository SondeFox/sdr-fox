#!/usr/bin/env python3
# SPDX-License-Identifier: MIT OR Apache-2.0
"""Reviewed reconstruction subjects; no caller-selected source or manifest paths."""
from dataclasses import dataclass
import hashlib
import json
from pathlib import Path
import re


SOURCE_ROOT = "7bcb45cd2a993240abe7f41dcaeec4a84bc04aeb"
LOCK_SHA256 = "62ab5b79776c322f9776c8c41f8a6707fac6dfcf6a49b791fcb30e10999b4f76"
MANIFEST_ROOT = Path(__file__).resolve().parent


@dataclass(frozen=True)
class Profile:
    name: str
    source_commit: str
    source_tree: str
    manifest_name: str
    developer_directory: str
    evidence_kind: str
    xcode_identity: str | None = None
    android_link_args: tuple[str, ...] = ()


HISTORICAL = Profile(
    "current-pin", "fb34d8c600725b54c5a950234c892a593c343968",
    "fb5fcf5ce09c8c31bb4004b5861d4533c37b581d", "expected.json",
    "/Applications/Xcode_26.6.app/Contents/Developer",
    "current-pin-artifact-verification",
)
ANDROID_PAGE_SIZE = Profile(
    "android16kb-2d257275", "2d25727523646c166771f066634b16f60ce22977",
    "021353fabc37dca936cede81b4ce54808c4e5c83", "android-page-size-expected.json",
    "/Applications/Xcode_27.0.app/Contents/Developer",
    "android-page-size-candidate-verification", "Xcode 27.0\nBuild version 27A266a",
    ("-Wl,-z,max-page-size=16384", "-Wl,-z,common-page-size=16384"),
)
PROFILES = (HISTORICAL, ANDROID_PAGE_SIZE)


def select_profile(name):
    for profile in PROFILES:
        if name == profile.name:
            return profile
    raise ValueError("Unknown reconstruction profile")


def _document(raw):
    def unique(pairs):
        result = {}
        for key, value in pairs:
            if key in result:
                raise ValueError("Duplicate expected-manifest key")
            result[key] = value
        return result

    def invalid_number(_):
        raise ValueError("Non-finite expected-manifest number")

    return json.loads(raw, object_pairs_hook=unique, parse_constant=invalid_number)


@dataclass(frozen=True)
class Manifest:
    profile: Profile
    raw: bytes

    def __post_init__(self):
        if self.profile not in PROFILES or not 0 < len(self.raw) <= 2 * 1024 * 1024:
            raise ValueError("Unreviewed profile or invalid manifest size")
        data = _document(self.raw)
        if type(data) is not dict or type(data.get("schema")) is not int or data["schema"] != 1:
            raise ValueError("Unsupported expected-manifest schema")
        source = data.get("source")
        if type(source) is not dict or type(data.get("toolchain")) is not dict:
            raise ValueError("Missing expected-manifest identities")
        artifacts = data.get("artifacts")
        paths = {"macos_archive": "libsdr_fox_ffi.a", "c_header": "sdr_fox.h", "kotlin_binding": "SdrFox.kt",
                 "android_arm64_jni": "arm64-v8a/libsdr_fox_jni.so", "android_x86_64_jni": "x86_64/libsdr_fox_jni.so"}
        if type(artifacts) is not list or len(artifacts) != 5:
            raise ValueError("Manifest must bind all five outputs")
        if any(type(item) is not dict or type(item.get("role")) is not str for item in artifacts):
            raise ValueError("Invalid manifest artifact identity")
        if {item["role"]: item.get("path") for item in artifacts} != paths:
            raise ValueError("Unexpected manifest artifact layout")
        for item in artifacts:
            if (type(item.get("size_bytes")) is not int or not 0 < item["size_bytes"] <= 2 * 1024 ** 3 or
                    type(item.get("sha256")) is not str or re.fullmatch(r"[0-9a-f]{64}", item["sha256"]) is None):
                raise ValueError("Invalid manifest artifact bytes")
        expected_source = {
            "commit": self.profile.source_commit, "tree": self.profile.source_tree,
            "verified_fresh_history_root": SOURCE_ROOT, "cargo_lock_sha256": LOCK_SHA256,
        }
        if any(source.get(key) != value for key, value in expected_source.items()):
            raise ValueError("Manifest source differs from selected profile")
        if source.get("repository") != "https://github.com/SondeFox/sdr-fox.git":
            raise ValueError("Manifest source is not canonical")
        if self.profile != HISTORICAL:
            if data.get("reconstruction_profile") != self.profile.name:
                raise ValueError("Manifest profile identity mismatch")
            if data["toolchain"].get("xcode_identity") != self.profile.xcode_identity:
                raise ValueError("Manifest Xcode identity mismatch")
            if data.get("android_link_args") != list(self.profile.android_link_args):
                raise ValueError("Manifest JNI link policy mismatch")

    @property
    def document(self):
        # Return a new parse so a consumer cannot alter the data bound by raw.
        return _document(self.raw)

    @property
    def sha256(self):
        return hashlib.sha256(self.raw).hexdigest()


def load_manifest(profile=HISTORICAL):
    if profile not in PROFILES:
        raise ValueError("Unreviewed reconstruction profile")
    path = MANIFEST_ROOT / profile.manifest_name
    if path.is_symlink() or not path.is_file() or path.stat().st_size > 2 * 1024 * 1024:
        raise ValueError("Expected manifest unavailable")
    return Manifest(profile, path.read_bytes())
