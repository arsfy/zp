#!/usr/bin/env python3
"""Validate release tags, sign the updater manifest and prepare release notes."""

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


def release_base_url(version):
    return f"https://github.com/arsfy/zp/releases/download/v{version}"


def release_packages(version):
    """Group packages by OS, with their updater kind and optional download label."""
    macos = []
    for arch, label in [("aarch64", "ARM"), ("x86_64", "x86_64")]:
        platform = f"darwin-{arch}"
        stem = f"ZP-{version}-{platform}"
        macos.extend([
            (platform, "mac-app", f"{stem}.zip", None),
            (platform, None, f"{stem}.dmg", label),
        ])

    linux = []
    for arch, debarch, label in [("x86_64", "amd64", "x86_64"), ("aarch64", "arm64", "ARM64")]:
        platform = f"linux-{arch}"
        stem = f"ZP-{version}-{platform}"
        linux.extend([
            (platform, "archive", f"{stem}.tar.gz", label),
            (platform, "app-image", f"{stem}.AppImage", f"{label} AppImage"),
            # GitHub would rename `~`, so it stays inside the package metadata.
            (platform, "deb", f"zp_{version}_{debarch}.deb", f"{label} deb"),
        ])

    windows = f"ZP-{version}-windows-x86_64"
    return {
        "macOS": macos,
        "Linux": linux,
        "Windows": [
            ("windows-x86_64", "windows-portable", f"{windows}.zip", "x86_64"),
            ("windows-x86_64", "windows-installer", f"{windows}-setup.exe", "x86_64 Setup"),
        ],
    }


def notes_with_downloads(notes, version):
    base = release_base_url(version)
    lines = ["#### Downloads"]
    for os_name, packages in release_packages(version).items():
        lines.extend(["", f"##### {os_name}", ""])
        lines.extend(
            f"- {label}: [{name}]({base}/{name})"
            for _, _, name, label in packages
            if label is not None
        )
    downloads = "\n".join(lines) + "\n"
    notes = notes.rstrip("\n")
    return f"{notes}\n\n{downloads}" if notes.strip() else downloads


def generate(directory, version, channel):
    base = release_base_url(version)
    platforms = {}

    for packages in release_packages(version).values():
        for platform, kind, name, _ in packages:
            path = directory / name
            if not path.is_file() or not (size := path.stat().st_size):
                raise ValueError(f"Missing or empty release artifact: {name}")
            if kind is None:
                continue
            with path.open("rb") as artifact:
                digest = hashlib.file_digest(artifact, "sha256").hexdigest()
            platforms.setdefault(platform, []).append({
                "name": name, "kind": kind, "url": f"{base}/{name}",
                "size": size, "sha256": digest,
            })
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
    parser.add_argument("--notes-output", type=Path, help="write release notes with download links")
    args = parser.parse_args()
    version, channel = release_version()
    if args.validate:
        print(f"version={version}\nchannel={channel}\nprerelease={str(channel == 'preview').lower()}")
        return
    directory = Path(args.dist)
    manifest = directory / "release.json"
    release = generate(directory, version, channel)
    manifest.write_text(json.dumps(release, indent=2) + "\n")
    sign(manifest)
    if args.notes_output:
        args.notes_output.write_text(notes_with_downloads(release["notes"], version))
    print(f"Signed release manifest for ZP {version} ({channel})")


if __name__ == "__main__":
    main()
