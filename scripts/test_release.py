"""Offline checks for release tags, package manifests, download links and signatures."""

from contextlib import contextmanager
import base64
import json
import os
from pathlib import Path
import subprocess
import tempfile
import unittest
from unittest.mock import patch

import release


@contextmanager
def working_directory(path):
    previous = Path.cwd()
    os.chdir(path)
    try:
        yield
    finally:
        os.chdir(previous)


class ReleaseTests(unittest.TestCase):
    def test_tag_validation_accepts_stable_preview_and_rc(self):
        with tempfile.TemporaryDirectory() as directory, working_directory(directory):
            for version in ["0.3.0", "0.3.0-preview.1", "0.3.0-rc.10"]:
                Path("Cargo.toml").write_text(f'[workspace.package]\nversion = "{version}"\n')
                with patch.dict(os.environ, {"ZP_RELEASE_TAG": f"v{version}"}):
                    self.assertEqual(release.release_version(), (version, "preview" if "-" in version else "stable"))
                with patch.dict(os.environ, {"ZP_RELEASE_TAG": "v9.0.0"}):
                    with self.assertRaises(ValueError):
                        release.release_version()
            for version in ["0.3", "0.3.0-beta.1", "0.3.0-rc.01", "0.3.0-rc"]:
                Path("Cargo.toml").write_text(f'[workspace.package]\nversion = "{version}"\n')
                with self.assertRaises(ValueError):
                    release.release_version()

    def test_manifest_requires_all_packages_with_github_safe_names(self):
        version = "0.3.0-rc.1"
        with tempfile.TemporaryDirectory() as directory:
            directory = Path(directory)
            names = [f"ZP-{version}-darwin-{arch}.{extension}" for arch in ["aarch64", "x86_64"] for extension in ["zip", "dmg"]]
            for arch, debarch in [("x86_64", "amd64"), ("aarch64", "arm64")]:
                names.extend([
                    f"ZP-{version}-linux-{arch}.AppImage",
                    f"ZP-{version}-linux-{arch}.tar.gz",
                    f"zp_{version}_{debarch}.deb",
                ])
            names.extend([
                f"ZP-{version}-windows-x86_64-setup.exe",
                f"ZP-{version}-windows-x86_64.zip",
            ])
            for name in names:
                (directory / name).write_bytes(b"package")
            with patch.dict(os.environ, {"ZP_RELEASE_NOTES": 'A "quoted" note\nwith a newline', "ZP_RELEASE_NOTES_FILE": ""}):
                manifest = release.generate(directory, version, "preview")
            self.assertEqual(json.loads(json.dumps(manifest))["notes"], 'A "quoted" note\nwith a newline')
            for packages in manifest["platforms"].values():
                for package in packages:
                    self.assertEqual(package["size"], 7)
                    self.assertEqual(len(package["sha256"]), 64)
                    self.assertIn(f"/v{version}/", package["url"])
            for arch, debarch in [("x86_64", "amd64"), ("aarch64", "arm64")]:
                self.assertEqual(
                    {package["kind"]: package["name"] for package in manifest["platforms"][f"linux-{arch}"]},
                    {
                        "archive": f"ZP-{version}-linux-{arch}.tar.gz",
                        "app-image": f"ZP-{version}-linux-{arch}.AppImage",
                        "deb": f"zp_{version}_{debarch}.deb",
                    },
                )
            for name in [names[0], f"zp_{version}_arm64.deb", f"ZP-{version}-darwin-aarch64.dmg"]:
                with self.subTest(missing=name):
                    (directory / name).unlink()
                    with self.assertRaisesRegex(ValueError, "Missing or empty release artifact"):
                        release.generate(directory, version, "preview")
                    (directory / name).write_bytes(b"package")

    def test_downloads_follow_the_original_release_notes(self):
        version = "0.3.0-rc.1"
        original = "## What's Changed\n\n- Example change\n"
        notes = release.notes_with_downloads(original, version)
        self.assertTrue(notes.startswith(original + "\n#### Downloads\n"))
        self.assertEqual(
            [line for line in notes.splitlines() if line.startswith("##### ")],
            ["##### macOS", "##### Linux", "##### Windows"],
        )
        expected = [
            ("ARM", f"ZP-{version}-darwin-aarch64.dmg"),
            ("x86_64", f"ZP-{version}-darwin-x86_64.dmg"),
            ("x86_64", f"ZP-{version}-linux-x86_64.tar.gz"),
            ("x86_64 AppImage", f"ZP-{version}-linux-x86_64.AppImage"),
            ("x86_64 deb", f"zp_{version}_amd64.deb"),
            ("ARM64", f"ZP-{version}-linux-aarch64.tar.gz"),
            ("ARM64 AppImage", f"ZP-{version}-linux-aarch64.AppImage"),
            ("ARM64 deb", f"zp_{version}_arm64.deb"),
            ("x86_64", f"ZP-{version}-windows-x86_64.zip"),
            ("x86_64 Setup", f"ZP-{version}-windows-x86_64-setup.exe"),
        ]
        downloads = notes.split("#### Downloads\n", 1)[1]
        links = [
            f"- {label}: [{name}](https://github.com/arsfy/zp/releases/download/v{version}/{name})"
            for label, name in expected
        ]
        self.assertEqual(
            [line for line in downloads.splitlines() if line.startswith("- ")],
            links,
        )
        self.assertTrue(notes.endswith(links[-1] + "\n"))
        self.assertTrue(release.notes_with_downloads("", version).startswith("#### Downloads\n"))

    def test_signature_matches_embedded_public_key(self):
        with tempfile.TemporaryDirectory() as directory:
            directory = Path(directory)
            key = directory / "key.pem"
            subprocess.run(["openssl", "genpkey", "-algorithm", "Ed25519", "-out", str(key)], check=True, capture_output=True)
            public = subprocess.check_output(["openssl", "pkey", "-in", str(key), "-pubout", "-outform", "DER"])
            manifest = directory / "release.json"
            manifest.write_text('{"schema_version":1}')
            with patch.dict(os.environ, {"ZP_UPDATE_PRIVATE_KEY": key.read_text(), "ZP_UPDATE_PUBLIC_KEY": base64.b64encode(public[-32:]).decode()}):
                release.sign(manifest)
                self.assertEqual(manifest.with_suffix(".json.sig").stat().st_size, 64)
            with patch.dict(os.environ, {"ZP_UPDATE_PRIVATE_KEY": key.read_text(), "ZP_UPDATE_PUBLIC_KEY": base64.b64encode(bytes(32)).decode()}):
                with self.assertRaises(ValueError):
                    release.sign(manifest)


if __name__ == "__main__":
    unittest.main()
