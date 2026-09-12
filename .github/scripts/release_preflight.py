"""Release-only metadata and rendered documentation gates (Python >= 3.11).

Exit 1 is a source defect; exit 2 is inconclusive network validation.
No publication, configuration changes, or broad external link crawling.
"""

import argparse
import hashlib
from html.parser import HTMLParser
from pathlib import Path
import re
import shutil
import sys
import tomllib
from urllib.error import HTTPError, URLError
from urllib.parse import unquote, urldefrag, urlsplit
from urllib.request import Request, urlopen
from zipfile import ZipFile

PRODUCTS = ("limpid", "limpidctl", "limpid-prometheus", "limpid-metrics-schema", "limpid-windows")
REPOSITORY_PREFIX = "https://github.com/naoto256/limpid/blob/"


class SourceDefect(Exception):
    pass


class NetworkUnavailable(Exception):
    pass


def require(condition, message):
    if not condition:
        raise SourceDefect(message)


def read_toml(path):
    return tomllib.loads(path.read_text(encoding="utf-8"))


def release_assets(incoming, output, version):
    """Validate before flattening; never overwrite a prior publication staging area."""
    # This is the complete distribution contract, not a glob of whatever happened
    # to build. Changing the build matrix also requires updating this set.
    expected = {
        f"limpid_{version}-1_amd64.deb",
        f"limpid-prometheus_{version}-1_amd64.deb",
        f"limpid-{version}-x86_64-pc-windows-msvc.zip",
        f"limpid-{version}-aarch64-pc-windows-msvc.zip",
    }
    files = [p for p in incoming.rglob("*") if not p.is_dir()]
    names = [p.name for p in files]
    require(len(names) == len(set(names)), "Duplicate release asset basename")
    require(set(names) == expected, f"Asset set mismatch: missing={sorted(expected - set(names))}, extra={sorted(set(names) - expected)}")
    require(not output.exists(), "Asset output already exists")
    for path in files:
        require(path.is_file() and not path.is_symlink() and path.stat().st_size > 0, f"Invalid asset: {path.name}")
        if path.suffix == ".zip":
            with ZipFile(path) as archive:
                entries = archive.namelist()
                require(len(entries) == len(set(entries)), "Duplicate ZIP entry")
                binaries = {"limpid.exe", "limpidctl.exe", "limpid-prometheus.exe"}
                required = binaries | {"SHA256SUMS", "install.ps1", "uninstall.ps1", "limpid.conf.example", "README.md"}
                require(required <= set(entries), f"Incomplete Windows ZIP: {path.name}")
                require(any(n.startswith("snippets/") for n in entries), "ZIP snippets missing")
                checksums = archive.read("SHA256SUMS").decode("utf-8").splitlines()
                parsed = [line.split("  ", 1) for line in checksums]
                require(len(parsed) == 3 and all(len(p) == 2 for p in parsed), "Invalid binary checksum manifest")
                require({p[1] for p in parsed} == binaries, "Binary checksum set mismatch")
                for digest, name in parsed:
                    binary = archive.read(name)
                    require(hashlib.sha256(binary).hexdigest() == digest.lower(), f"Binary checksum mismatch: {name}")
                    # A successful --version on ARM can also be an emulated x64 binary.
                    # Bind the archive target to the PE machine, not the runner label alone.
                    pe_offset = int.from_bytes(binary[60:64], "little")
                    machine = 0xAA64 if "aarch64" in path.name else 0x8664
                    require(binary[:2] == b"MZ" and binary[pe_offset:pe_offset + 4] == b"PE\0\0"
                            and int.from_bytes(binary[pe_offset + 4:pe_offset + 6], "little") == machine,
                            f"PE target mismatch: {path.name}/{name}")
    output.mkdir(parents=True)
    manifest = []
    for path in sorted(files, key=lambda p: p.name):
        shutil.copyfile(path, output / path.name)
        manifest.append(f"{hashlib.sha256(path.read_bytes()).hexdigest()}  {path.name}\n")
    (output / "SHA256SUMS").write_text("".join(manifest), encoding="utf-8")


def release_metadata(root, version):
    manifests = {name: read_toml(root / "crates" / name / "Cargo.toml") for name in PRODUCTS}
    version = version or manifests["limpid"]["package"]["version"]
    require(re.fullmatch(r"[0-9]+\.[0-9]+\.[0-9]+", version), "Expected a stable X.Y.Z release version")
    for name, manifest in manifests.items():
        require(manifest["package"]["version"] == version, f"Version mismatch: {name}")
    workspace_schema = read_toml(root / "Cargo.toml")["workspace"]["dependencies"]["limpid-metrics-schema"]
    require(workspace_schema["version"] == version, "Workspace schema requirement mismatch")
    for name in ("limpid", "limpidctl", "limpid-prometheus"):
        dependency = manifests[name]["dependencies"]["limpid-metrics-schema"]
        require(dependency.get("workspace") is True, f"Schema requirement must inherit workspace: {name}")
    packages = read_toml(root / "Cargo.lock")["package"]
    for name in PRODUCTS:
        versions = [p["version"] for p in packages if p["name"] == name]
        require(versions == [version], f"Lock version mismatch: {name}")
    lines = (root / "CHANGELOG.md").read_text(encoding="utf-8").splitlines(keepends=True)
    starts = [i for i, line in enumerate(lines) if re.match(r"^## \[" + re.escape(version) + r"\]", line)]
    require(len(starts) == 1, f"Expected exactly one CHANGELOG section for {version}")
    section = []
    for line in lines[starts[0] + 1:]:
        if line.startswith("## ["):
            break
        section.append(line)
    body = "".join(section)
    require(body.strip(), "Release notes are empty")
    tagline = ""
    for line in section:
        if line.startswith("##"):
            break
        if line.startswith(">"):
            tagline = re.sub(r"^> ?", "", line).rstrip("\r\n")
            break
    title = f"limpid {version}" + (f" — {tagline}" if tagline else "")
    return version, title, body


class Page(HTMLParser):
    def __init__(self, text):
        super().__init__(convert_charrefs=True)
        self.ids = set()
        self.links = []
        self.feed(text)

    def handle_starttag(self, tag, attributes):
        attrs = dict(attributes)
        if attrs.get("id"):
            self.ids.add(attrs["id"])
        if tag == "a" and attrs.get("name"):
            self.ids.add(attrs["name"])
        for key in ("href", "src"):
            if attrs.get(key):
                self.links.append(attrs[key])


def check_book(book):
    book = book.resolve()
    require((book / "index.html").is_file(), "Generated book entry index.html is missing")
    pages = {p.resolve(): Page(p.read_text(encoding="utf-8")) for p in book.rglob("*.html") if p.name != "print.html"}
    require(pages, "No generated mdBook HTML found")
    repository_links = set()
    errors = []
    for path, page in pages.items():
        for href in page.links:
            url = urlsplit(href)
            if href.startswith(REPOSITORY_PREFIX):
                # Deliberately selected book-external document references only.
                destination = url.path
                if destination.endswith(("/CHANGELOG.md", "/packaging/snippets/README.md")):
                    repository_links.add(href)
            if url.scheme or url.netloc:
                continue
            target = (book / unquote(url.path).lstrip("/") if url.path.startswith("/") else path.parent / unquote(url.path)).resolve() if url.path else path
            if target.is_dir():
                target = target / "index.html"
            if not target.is_relative_to(book) or not target.is_file():
                errors.append(f"{path.relative_to(book)}: missing target {href}")
                continue
            if url.fragment and target.suffix == ".html":
                parsed = pages.get(target) or Page(target.read_text(encoding="utf-8"))
                if unquote(url.fragment) not in parsed.ids:
                    errors.append(f"{path.relative_to(book)}: missing anchor {href}")
    require(not errors, "\n".join(errors))
    return repository_links


def fetch_page(url):
    with urlopen(Request(url, headers={"User-Agent": "limpid-release-preflight"}), timeout=20) as response:
        return response.read().decode("utf-8")


def check_repository_links(links, fetch=fetch_page):
    cache = {}
    for link in sorted(links):
        url, fragment = urldefrag(link)
        try:
            if url not in cache:
                cache[url] = Page(fetch(url))
        except HTTPError as error:
            error.close()
            if error.code in (404, 410):
                raise SourceDefect(f"Repository document destination absent: {url}") from error
            raise NetworkUnavailable(f"Repository check inconclusive: HTTP {error.code} for {url}") from error
        except (URLError, TimeoutError, OSError) as error:
            raise NetworkUnavailable(f"Repository check inconclusive: {url}") from error
        if fragment:
            fragment = unquote(fragment)
            page = cache[url]
            require(fragment in page.ids or "user-content-" + fragment in page.ids or "#" + fragment in page.links,
                    f"Repository document anchor absent: {link}")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--root", type=Path, default=Path(__file__).resolve().parents[2])
    parser.add_argument("--version", help="Exact release version; default: limpid manifest version")
    parser.add_argument("--book", type=Path, help="Already generated mdBook directory")
    parser.add_argument("--check-remote", action="store_true", help="Validate selected repository links, not arbitrary external sites")
    parser.add_argument("--notes-output", type=Path)
    parser.add_argument("--github-output", type=Path)
    parser.add_argument("--assets", type=Path, help="Per-job artifact directories to validate")
    parser.add_argument("--asset-output", type=Path, help="New flat directory for validated release assets")
    args = parser.parse_args()
    try:
        version, title, body = release_metadata(args.root, args.version)
        require(bool(args.assets) == bool(args.asset_output), "--assets and --asset-output are required together")
        if args.assets:
            release_assets(args.assets, args.asset_output, version)
        if args.check_remote and not args.book:
            raise SourceDefect("--check-remote requires --book")
        if args.book:
            links = check_book(args.book)
            if args.check_remote:
                check_repository_links(links)
        if args.notes_output:
            args.notes_output.write_text(body, encoding="utf-8")
        if args.github_output:
            with args.github_output.open("a", encoding="utf-8") as output:
                output.write(f"name={title}\n")
        print(f"Release {version}: {title}")
        print(f"Notes SHA256: {hashlib.sha256(body.encode()).hexdigest()}")
    except NetworkUnavailable as error:
        print(f"INCONCLUSIVE NETWORK: {error}", file=sys.stderr)
        return 2
    except (SourceDefect, KeyError, ValueError, OSError) as error:
        print(f"RELEASE PREFLIGHT FAILED: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
