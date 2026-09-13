# limpid website

This site builds static HTML for GitHub Pages at
https://naoto256.github.io/limpid/. It does not publish through another hosting service.

The shared social card is `src/og.png` (1200 × 630), copied unchanged into the build.
`og-source.svg` is its editable source, not a build input or a served asset.
The approved PNG was exported with CairoSVG using Helvetica Neue; rendering the
SVG with different installed fonts can change its appearance. Keep the approved
PNG unless a replacement is reviewed. No font or image-rendering dependency is
needed to build the site. Social platforms may cache or crop the shared card.

```sh
cd site
# Use the exact Node version in .node-version.
npm ci --ignore-scripts
npm run dev
```

Development: http://localhost:8088/limpid/ (output `.dev`).

```sh
npm run build
npm run check
npm run check:links
npm run preview
```

Production artifact: http://127.0.0.1:8089/limpid/ (output `dist`).
For a future custom domain, set `SITE_BASE=/` and `SITE_ORIGIN=https://your-domain.example`
for both build and preview. The default origin is `https://naoto256.github.io`.
Neither command deploys. Static rendering requires Node, not the Rust daemon or
native Kafka/journal dependencies. Commit the npm lockfile with dependency changes.

## Content ownership

`docs/src` remains the documentation source; `SUMMARY.md` supplies navigation.
The renderer reads all 50 chapters directly, preserving code fences and copying
the existing images. No generated Markdown is checked in. `packaging/snippets`
and its xtask-managed inventory remain in their existing locations.

The current site targets stable 0.9.0. Runtime and snippet links resolve to
`v0.9.0`; documentation's Read source links use the checked-out Git commit.
Build publication artifacts only from a clean checkout: uncommitted local preview
edits are not represented by that commit link. Publishing content for another
version requires an explicit content review and a version update in `lib/config.js`.
The five-package version assertion (including `limpid-windows`) catches version mismatches, not semantic
unreleased-feature drift; review the exact content before publication. There is no next site.

The Recipes index and detail pages use `/recipes/`; `/docs/pipelines/` remains
the DSL pipeline reference. The fourteen recipe sources live in `src/`: archive, filtering and thinning, branching, safe forwarding, quarantine, asset enrichment, Loki, Elasticsearch, Datadog,
Better Stack, New Relic, CloudWatch, AMA, and CEF to AMP. Archival routes every sender to a file; filtering
and table-based suppression are separate examples in Recipe 02. Recipes are
authored configurations with receiver prerequisites; changes must preserve their
actual validation boundaries. Rendering is not an integration test.
The home-page DSL is the existing README pipeline fragment.

### Running the new recipe examples

On a Unix host, opt into the real-process checks with explicit 0.9.0 binaries:

```sh
LIMPID_BIN=/path/to/limpid LIMPIDCTL_BIN=/path/to/limpidctl node test/recipes-live.mjs
```

This reads configuration, fixture and injection fences from New Relic, safe forwarding,
quarantine and asset enrichment. It uses private temporary directories and loopback
receivers, checks output contents and counts, and requires normal daemon exit.
Setup and test failures run all acquired-resource cleanup. A daemon that misses
the 10-second normal-stop deadline still fails the test: the harness sends SIGKILL
only to its own spawned child and waits up to 5 seconds for exit confirmation.
An unconfirmed exit is reported as an additional cleanup failure; cleanup errors
do not hide the original failure. No existing service, arbitrary PID or process
group is targeted.
The evidence directory records article hashes and results. It does not contact cloud
services or prove provider-side storage, authentication or TLS. It is separate from
the static site tests and does not yet cover the older recipes.

## Publication

The site workflow checks PRs without Pages permissions or setup. Deployment is
manual (`workflow_dispatch`) from `main` only, after Owner merge approval and
GitHub Pages/`github-pages` environment setup. The deploy job consumes the same
run's verified artifact; no HTML or `gh-pages` branch is committed.
Build logs record the source commit and artifact checksums.

Every page has a canonical URL and is listed in `sitemap.xml`; no `noindex` is
emitted. A project-local `/limpid/robots.txt` cannot control origin-root crawling,
so none is generated. Check the origin's `/robots.txt` and live URLs at first
publication; this project does not modify the account-level site.

The existing Rust CI remains unchanged. The release workflow adds native Windows
x64/ARM64 builds and ZIP packaging alongside Debian, then verifies the complete
asset set and checksums before a single tag-only publish job. mdBook is retained
for migration comparison until the site is accepted; it is not a second content
source. Remove its renderer/config only as part of the approved cutover.
