import { readdirSync, readFileSync, existsSync } from "node:fs";
import path from "node:path";
import { base, canonical } from "../lib/config.js";
const root = path.resolve(process.env.SITE_OUTPUT || "dist");
const files = readdirSync(root, { recursive: true }).filter((x) =>
  x.endsWith(".html"),
);
const failures = [];
const sitemap = readFileSync(path.join(root, "sitemap.xml"), "utf8");
const locations = [...sitemap.matchAll(/<loc>([^<]+)<\/loc>/g)].map(
  (m) => m[1],
);
if (
  new Set(locations).size !== files.length ||
  locations.length !== files.length
)
  failures.push("sitemap must contain every HTML page exactly once");
for (const file of files) {
  const html = readFileSync(path.join(root, file), "utf8");
  const expected = canonical(file);
  if (!locations.includes(expected))
    failures.push(`${file}: missing sitemap URL`);
  if (!html.includes(`rel="canonical" href="${expected}"`))
    failures.push(`${file}: incorrect canonical URL`);
  if (/<meta[^>]+(?:noindex|nofollow)/i.test(html))
    failures.push(`${file}: unexpected indexing restriction`);
  for (const [, href] of html.matchAll(/(?:href|src)="([^"]+)"/g)) {
    if (/^(?:[a-z]+:|\/\/)/i.test(href)) continue;
    const resolved = new URL(
      href.replaceAll("&amp;", "&"),
      `https://example.invalid${base}${file}`,
    );
    if (!resolved.pathname.startsWith(base)) {
      failures.push(`${file}: outside base ${href}`);
      continue;
    }
    let target = decodeURIComponent(resolved.pathname.slice(base.length));
    if (target.endsWith("/")) target += "index.html";
    if (!existsSync(path.join(root, target))) {
      failures.push(`${file}: missing ${href}`);
      continue;
    }
    if (resolved.hash && target.endsWith(".html")) {
      const content = readFileSync(path.join(root, target), "utf8");
      if (
        !content.includes(`id="${decodeURIComponent(resolved.hash.slice(1))}"`)
      )
        failures.push(`${file}: missing anchor ${href}`);
    }
  }
}
if (failures.length) {
  console.error(failures.join("\n"));
  process.exitCode = 1;
}
console.log(
  `${files.length} HTML pages; ${failures.length} broken local links/anchors`,
);
