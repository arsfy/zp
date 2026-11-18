#!/usr/bin/env python3
"""Validate release tags and generate the signed updater manifest."""

import argparse
import base64
from datetime import datetime, timezone
import hashlib
import json
import os
from pathlib import Path
import re
import subprocess
import tempfile
import tomllib


def release_version():
    with Path("Cargo.toml").open("rb") as manifest:
        version = tomllib.load(manifest)["workspace"]["package"]["version"]
    number = r"(?:0|[1-9][0-9]*)"
    if not re.fullmatch(rf"{number}\.{number}\.{number}(?:-(preview|rc)\.{number})?", version):
        raise ValueError("Release versions must be X.Y.Z, X.Y.Z-preview.N or X.Y.Z-rc.N")
    tag = os.environ.get("ZP_RELEASE_TAG", os.environ.get("GITHUB_REF_NAME", f"v{version}"))
    if tag != f"v{version}":
        raise ValueError(f"Tag {tag} does not match Cargo version v{version}")
    return version, "preview" if "-" in version else "stable"


def generate(directory, version, channel):
    base = f"https://github.com/arsfy/zp/releases/download/v{version}"
    platforms = {}

    def add(platform, kind, name):
        path = directory / name
        if not path.is_file() or not path.stat().st_size:
            raise ValueError(f"Missing or empty release artifact: {name}")
        digest = hashlib.sha256()
        with path.open("rb") as artifact:
            for chunk in iter(lambda: artifact.read(1024 * 1024), b""):
                digest.update(chunk)
        platforms.setdefault(platform, []).append({
            "name": name, "kind": kind, "url": f"{base}/{name}",
            "size": path.stat().st_size, "sha256": digest.hexdigest(),
        })

    for arch in ["aarch64", "x86_64"]:
        add(f"darwin-{arch}", "mac-app", f"ZP-{version}-darwin-{arch}.zip")
        if not (directory / f"ZP-{version}-darwin-{arch}.dmg").is_file():
            raise ValueError(f"Missing macOS {arch} DMG")
    add("linux-x86_64", "app-image", f"ZP-{version}-linux-x86_64.AppImage")
    add("linux-x86_64", "archive", f"ZP-{version}-linux-x86_64.tar.gz")
    # Debian's `~` stays inside the package: GitHub would rename it in an asset name.
    add("linux-x86_64", "deb", f"zp_{version}_amd64.deb")
    add("windows-x86_64", "windows-installer", f"ZP-{version}-windows-x86_64-setup.exe")
    add("windows-x86_64", "windows-portable", f"ZP-{version}-windows-x86_64.zip")
    notes_file = os.environ.get("ZP_RELEASE_NOTES_FILE")
    notes = Path(notes_file).read_text() if notes_file else os.environ.get("ZP_RELEASE_NOTES", "")
    return {
        "schema_version": 1, "version": version, "channel": channel,
        "published_at": datetime.now(timezone.utc).isoformat().replace("+00:00", "Z"),
        "notes": notes, "platforms": platforms,
    }


def sign(path):
    private_key = os.environ.get("ZP_UPDATE_PRIVATE_KEY", "")
    public_key = os.environ.get("ZP_UPDATE_PUBLIC_KEY", "")
    if not private_key or not public_key:
        raise ValueError("ZP_UPDATE_PRIVATE_KEY and ZP_UPDATE_PUBLIC_KEY are required")
    expected = base64.b64decode(public_key.strip(), validate=True)
    if len(expected) != 32:
        raise ValueError("ZP_UPDATE_PUBLIC_KEY must contain the base64-encoded 32-byte Ed25519 public key")
    with tempfile.TemporaryDirectory() as directory:
        key = Path(directory) / "key.pem"
        key.write_text(private_key)
        key.chmod(0o600)
        derived = subprocess.check_output(["openssl", "pkey", "-in", str(key), "-pubout", "-outform", "DER"])
        if derived != bytes.fromhex("302a300506032b6570032100") + expected:
            raise ValueError("The release signing key does not match the embedded public key")
        subprocess.run(["openssl", "pkeyutl", "-sign", "-rawin", "-inkey", str(key),
                        "-in", str(path), "-out", str(path.with_suffix(".json.sig"))], check=True)


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--validate", action="store_true")
    parser.add_argument("--dist", default="dist")
    args = parser.parse_args()
    version, channel = release_version()
    if args.validate:
        print(f"version={version}\nchannel={channel}\nprerelease={str(channel == 'preview').lower()}")
        return
    directory = Path(args.dist)
    manifest = directory / "release.json"
    manifest.write_text(json.dumps(generate(directory, version, channel), indent=2) + "\n")
    sign(manifest)
    print(f"Signed release manifest for ZP {version} ({channel})")


if __name__ == "__main__":
    main()
