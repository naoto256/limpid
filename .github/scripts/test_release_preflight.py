import importlib.util
from pathlib import Path
import tempfile
import unittest
import hashlib
from zipfile import ZipFile
from urllib.error import HTTPError, URLError

spec = importlib.util.spec_from_file_location("release_preflight", Path(__file__).with_name("release_preflight.py"))
preflight = importlib.util.module_from_spec(spec)
spec.loader.exec_module(preflight)
REPO = Path(__file__).resolve().parents[2]


class ReleasePreflightTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.root = Path(self.temp.name)
        for name in preflight.PRODUCTS:
            target = self.root / "crates" / name / "Cargo.toml"
            target.parent.mkdir(parents=True, exist_ok=True)
            target.write_text(f'[package]\nname = "{name}"\nversion = "0.8.1"\n'
                              '[dependencies]\nlimpid-metrics-schema = { workspace = true }\n')
        (self.root / "Cargo.toml").write_text('[workspace.dependencies]\nlimpid-metrics-schema = { version = "0.8.1" }\n')
        (self.root / "Cargo.lock").write_text(''.join(
            f'[[package]]\nname = "{name}"\nversion = "0.8.1"\n' for name in preflight.PRODUCTS))
        (self.root / "CHANGELOG.md").write_text('## [Unreleased]\n\n## [0.8.1]\n\n> Tagline.\n\nOverview.\n\n### Fixed — Header keys\n\nFacts.\n\n## [0.8.0]\nOld facts.\n')

    def tearDown(self):
        self.temp.cleanup()

    def test_current_release_has_title_and_notes(self):
        version, title, body = preflight.release_metadata(self.root, None)
        self.assertTrue(title.startswith(f"limpid {version}"))
        self.assertIn("###", body)

    def test_repository_release_metadata(self):
        preflight.release_metadata(REPO, None)

    def asset_fixture(self):
        incoming = self.root / "incoming"
        incoming.mkdir()
        for name in ("limpid", "limpid-prometheus"):
            (incoming / f"{name}_0.8.1-1_amd64.deb").write_bytes(b"fixture deb")
        for target in ("x86_64", "aarch64"):
            with ZipFile(incoming / f"limpid-0.8.1-{target}-pc-windows-msvc.zip", "w") as archive:
                lines = []
                for name in ("limpid.exe", "limpidctl.exe", "limpid-prometheus.exe"):
                    machine = 0xAA64 if target == "aarch64" else 0x8664
                    payload = b"MZ" + bytes(58) + (64).to_bytes(4, "little") + b"PE\0\0" + machine.to_bytes(2, "little") + name.encode()
                    archive.writestr(name, payload)
                    lines.append(f"{hashlib.sha256(payload).hexdigest()}  {name}\n")
                archive.writestr("SHA256SUMS", "".join(lines))
                for name in ("install.ps1", "uninstall.ps1", "limpid.conf.example", "README.md", "snippets/example.limpid"):
                    archive.writestr(name, "fixture")
        return incoming

    def test_asset_set_and_outer_checksums(self):
        incoming = self.asset_fixture()
        output = self.root / "output"
        preflight.release_assets(incoming, output, "0.8.1")
        lines = (output / "SHA256SUMS").read_text().splitlines()
        self.assertEqual(len(lines), 4)
        for line in lines:
            digest, name = line.split("  ")
            self.assertEqual(digest, hashlib.sha256((output / name).read_bytes()).hexdigest())
        with self.assertRaises(preflight.SourceDefect):
            preflight.release_assets(incoming, output, "0.8.1")

    def test_asset_missing_extra_duplicate_rejected(self):
        incoming = self.asset_fixture()
        target = incoming / "limpid_0.8.1-1_amd64.deb"
        payload = target.read_bytes()
        target.unlink()
        with self.assertRaises(preflight.SourceDefect):
            preflight.release_assets(incoming, self.root / "output", "0.8.1")
        target.write_bytes(payload)
        extra = incoming / "extra"
        extra.write_bytes(b"extra")
        with self.assertRaises(preflight.SourceDefect):
            preflight.release_assets(incoming, self.root / "output", "0.8.1")
        extra.unlink()
        (incoming / "other-job").mkdir()
        (incoming / "other-job" / target.name).write_bytes(payload)
        with self.assertRaises(preflight.SourceDefect):
            preflight.release_assets(incoming, self.root / "output", "0.8.1")

    def test_directory_only_snippets_rejected(self):
        incoming = self.asset_fixture()
        path = incoming / "limpid-0.8.1-aarch64-pc-windows-msvc.zip"
        with ZipFile(path) as archive:
            entries = {name: archive.read(name) for name in archive.namelist()
                       if not name.startswith("snippets/")}
        with ZipFile(path, "w") as archive:
            for name, payload in entries.items():
                archive.writestr(name, payload)
            archive.writestr("snippets/", b"")
            archive.writestr("snippets/nested/", b"")
        with self.assertRaisesRegex(preflight.SourceDefect, "ZIP snippets missing"):
            preflight.release_assets(incoming, self.root / "output", "0.8.1")
        self.assertFalse((self.root / "output").exists())

    def test_corrupt_windows_binary_rejected(self):
        incoming = self.asset_fixture()
        path = incoming / "limpid-0.8.1-aarch64-pc-windows-msvc.zip"
        with ZipFile(path) as archive:
            entries = {name: archive.read(name) for name in archive.namelist()}
        entries["limpid.exe"] = b"changed"
        with ZipFile(path, "w") as archive:
            for name, payload in entries.items():
                archive.writestr(name, payload)
        with self.assertRaises(preflight.SourceDefect):
            preflight.release_assets(incoming, self.root / "output", "0.8.1")

    def test_wrong_architecture_with_valid_checksums_rejected(self):
        incoming = self.asset_fixture()
        x64 = incoming / "limpid-0.8.1-x86_64-pc-windows-msvc.zip"
        arm = incoming / "limpid-0.8.1-aarch64-pc-windows-msvc.zip"
        arm.write_bytes(x64.read_bytes())
        with self.assertRaises(preflight.SourceDefect):
            preflight.release_assets(incoming, self.root / "output", "0.8.1")

    def test_missing_or_whitespace_notes_fail(self):
        for text in ["## [Unreleased]\n", "## [0.8.1]\n \n## [0.8.0]\nold\n"]:
            (self.root / "CHANGELOG.md").write_text(text)
            with self.assertRaises(preflight.SourceDefect):
                preflight.release_metadata(self.root, "0.8.1")

    def test_tag_manifest_schema_and_lock_mismatch_fail(self):
        with self.assertRaises(preflight.SourceDefect):
            preflight.release_metadata(self.root, "99.0.0")
        for relative in ["crates/limpidctl/Cargo.toml", "crates/limpid-windows/Cargo.toml", "Cargo.toml", "Cargo.lock"]:
            path = self.root / relative
            original = path.read_text()
            path.write_text(original.replace('"0.8.1"', '"99.0.0"', 1))
            with self.assertRaises(preflight.SourceDefect):
                preflight.release_metadata(self.root, "0.8.1")
            path.write_text(original)

    def test_local_target_and_fragment(self):
        (self.root / "index.html").write_text('<a href="other.html#hello">ok</a>')
        (self.root / "other.html").write_text('<h2 id="hello">Hello</h2>')
        self.assertEqual(preflight.check_book(self.root), set())
        (self.root / "other.html").write_text('<h2 id="wrong">Hello</h2>')
        with self.assertRaises(preflight.SourceDefect):
            preflight.check_book(self.root)
        (self.root / "other.html").unlink()
        with self.assertRaises(preflight.SourceDefect):
            preflight.check_book(self.root)

    def test_readme_link_is_not_virtualized_to_index(self):
        (self.root / "index.html").write_text('<a href="README.html#intro">Introduction</a>')
        with self.assertRaises(preflight.SourceDefect):
            preflight.check_book(self.root)
        (self.root / "README.html").write_text('<h1 id="intro">Introduction</h1>')
        preflight.check_book(self.root)
        (self.root / "index.html").unlink()
        with self.assertRaises(preflight.SourceDefect):
            preflight.check_book(self.root)

    def test_deleted_branch_is_a_source_defect(self):
        url = "https://github.com/naoto256/limpid/blob/release/deleted/CHANGELOG.md"
        def missing(_):
            raise HTTPError(url, 404, "Not Found", {}, None)
        with self.assertRaises(preflight.SourceDefect):
            preflight.check_repository_links({url}, fetch=missing)

    def test_network_failure_is_not_a_source_defect(self):
        def unavailable(_):
            raise URLError("temporary DNS failure")
        with self.assertRaises(preflight.NetworkUnavailable):
            preflight.check_repository_links({preflight.REPOSITORY_PREFIX + "main/CHANGELOG.md"}, fetch=unavailable)

    def test_repository_fragment_and_selected_destinations(self):
        url = preflight.REPOSITORY_PREFIX + "main/packaging/snippets/README.md#authoring-conventions"
        preflight.check_repository_links({url}, fetch=lambda _: '<h2 id="user-content-authoring-conventions">ok</h2>')
        with self.assertRaises(preflight.SourceDefect):
            preflight.check_repository_links({url}, fetch=lambda _: '<h2 id="different">no</h2>')


if __name__ == "__main__":
    unittest.main()
