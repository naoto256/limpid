import test from "node:test";
import assert from "node:assert/strict";
import { pages } from "../lib/content.js";
import PageTemplate from "../src/pages.11ty.js";
import { readFileSync } from "node:fs";
import MarkdownIt from "markdown-it";

test("home keeps the approved concise copy and links to operational details", () => {
  const html = new PageTemplate().render({
    entry: pages().find((p) => p.kind === "home"),
  });
  assert.ok(
    html.includes(
      "Write reusable parsers and composers, then chain them together. Use the bundled snippets, adapt them, or write your own.",
    ),
  );
  assert.ok(
    html.includes(
      "Inspect events with tap. Follow pipeline activity and health through stats and metrics.",
    ),
  );
  assert.ok(
    html.includes(
      "Keep pending events in disk-backed queues and failures in an error log. Fix the cause, then replay with inject.",
    ),
  );
  assert.match(
    html,
    /docs\/snippets\/index.html">Explore the snippet library →/,
  );
  assert.match(html, /docs\/operations\/tap.html">Inspect events →/);
  assert.match(html, /docs\/operations\/metrics.html">Monitor your pipeline →/);
  assert.match(html, /error-log.html#replay/);
  assert.match(html, /recipes\/fortigate-cef-to-ocsf\//);
  assert.equal((html.match(/class="number"/g) || []).length, 3);
});

test("ten recipes remain grouped with the homepage example first", () => {
  const entries = pages();
  const html = new PageTemplate().render({
    entry: entries.find((p) => p.kind === "recipes"),
  });
  assert.match(html, /<h2>Processing<\/h2>/);
  assert.match(html, /<h2>Destinations<\/h2>/);
  assert.match(
    html,
    /class="recipe-list"><a href="[^"]*recipes\/fortigate-cef-to-ocsf\/">\s*<span class="number">00<\/span>/,
  );
  assert.ok(
    html.indexOf('class="number">00') < html.indexOf("<h2>Processing</h2>"),
  );
  assert.deepEqual(
    [...html.matchAll(/class="number">(\d+)<\/span>/g)].map((m) => m[1]),
    ["00", "01", "02", "03", "04", "05", "06", "07", "08", "09", "10"],
  );
  assert.equal(entries.filter((p) => p.kind === "recipe").length, 10);
  assert.ok(
    entries.find((p) => p.route === "recipes/fortigate-cef-to-ocsf/index.html"),
  );
});

test("homepage example keeps the README chain and renders the complete config literally", () => {
  const source = readFileSync("src/fortigate-cef-to-ocsf.md", "utf8");
  const original = readFileSync("../README.md", "utf8").match(
    /```limpid\n([\s\S]*?)```/,
  )[1];
  const config = source.match(/```limpid\n([\s\S]*?)```/)[1];
  assert.ok(config.includes(original.trimEnd()));
  assert.match(config, /type stdout/);
  const entry = pages().find((p) => p.kind === "example");
  const rendered = entry.content
    .match(/<pre><code[^>]*>([\s\S]*?)<\/code>/)[1]
    .replace(/<span class="[^"]+">|<\/span>/g, "");
  assert.equal(rendered, new MarkdownIt().utils.escapeHtml(config));
  assert.match(source, /neither connects to AWS Security Lake/);
});
