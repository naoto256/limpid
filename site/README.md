# limpid website

This site builds static HTML for GitHub Pages at
https://naoto256.github.io/limpid/. It does not publish through another hosting service.

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
The renderer reads all 48 chapters directly, preserving code fences and copying
the existing images. No generated Markdown is checked in. `packaging/snippets`
and its xtask-managed inventory remain in their existing locations.

The current site targets stable 0.8.4. Runtime and snippet links resolve to
`v0.8.4`; documentation's Read source links use the checked-out Git commit.
Build publication artifacts only from a clean checkout: uncommitted local preview
edits are not represented by that commit link. Publishing content for another
version requires an explicit content review and a version update in `lib/config.js`.
The four-package version assertion catches version mismatches, not semantic
unreleased-feature drift; review the exact content before publication. There is no next site.

The Recipes index and detail pages use `/recipes/`; `/docs/pipelines/` remains
the DSL pipeline reference. The ten recipe sources live in `src/`: archive, filtering and thinning, branching, Loki, Elasticsearch, Datadog,
Better Stack, CloudWatch, AMA, and CEF to AMP. Archival routes every sender to a file; filtering
and table-based suppression are separate examples in Recipe 02. Recipes are
authored configurations with receiver prerequisites; changes must preserve their
actual validation boundaries. Rendering is not an integration test.
The home-page DSL is the existing README pipeline fragment.

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

The existing Rust CI and release workflow remain unchanged. mdBook is retained
for migration comparison until the site is accepted; it is not a second content
source. Remove its renderer/config only as part of the approved cutover.
