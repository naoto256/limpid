import importlib.util
from pathlib import Path
import tempfile
import unittest
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

    def test_missing_or_whitespace_notes_fail(self):
        for text in ["## [Unreleased]\n", "## [0.8.1]\n \n## [0.8.0]\nold\n"]:
            (self.root / "CHANGELOG.md").write_text(text)
            with self.assertRaises(preflight.SourceDefect):
                preflight.release_metadata(self.root, "0.8.1")

    def test_tag_manifest_schema_and_lock_mismatch_fail(self):
        with self.assertRaises(preflight.SourceDefect):
            preflight.release_metadata(self.root, "99.0.0")
        for relative in ["crates/limpidctl/Cargo.toml", "Cargo.toml", "Cargo.lock"]:
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
