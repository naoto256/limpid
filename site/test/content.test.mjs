import test from "node:test";
import assert from "node:assert/strict";
import { readdirSync, readFileSync } from "node:fs";
import { pages, markdown, navigation, route } from "../lib/content.js";
import MarkdownIt from "markdown-it";
import PageTemplate from "../src/pages.11ty.js";
import { url, origin } from "../lib/config.js";

test("shared head includes exactly one static Search Console verification tag", () => {
  const tag =
    '<meta name="google-site-verification" content="S-EqEKp48UJAW41lZX5p1lCX1WOcv23Zq_XZxVzEsNk" />';
  const renderer = new PageTemplate();
  for (const entry of pages()) {
    const html = renderer.render({ entry });
    const head = html.match(/<head>([\s\S]*?)<\/head>/)[1];
    assert.equal(
      (html.match(/name="google-site-verification"/g) || []).length,
      1,
    );
    assert.ok(head.includes(tag), entry.route);
  }
});

test("Recipes navigation and routes stay separate from the Pipeline reference", () => {
  const entries = pages();
  const renderer = new PageTemplate();
  const index = entries.find((page) => page.route === "recipes/index.html");
  assert.equal(index.title, "Recipes");
  const html = renderer.render({ entry: index });
  assert.ok(html.includes(`href="${url("recipes/")}">Recipes</a>`));
  assert.ok(html.includes(`href="${url("docs/pipelines/index.html")}"`));
  for (const entry of entries) {
    assert.ok(!entry.route.startsWith("pipelines/"));
    if (entry.kind === "recipe") assert.ok(entry.route.startsWith("recipes/"));
  }
});

test("every page has an indexable canonical URL and docs link to the content commit", () => {
  const renderer = new PageTemplate();
  for (const entry of pages()) {
    const html = renderer.render({ entry });
    const pathname = entry.route.replace(/index\.html$/, "");
    assert.ok(
      html.includes(`rel="canonical" href="${origin}${url(pathname)}"`),
    );
    assert.ok(!/noindex/i.test(html));
    if (entry.kind === "docs") {
      assert.match(html, /blob\/[0-9a-f]{40}\/docs\/src\//);
    }
  }
});

test("filtering is second and archival never drops messages", () => {
  const recipes = pages().filter((page) => page.kind === "recipe");
  assert.equal(recipes[1].route, "recipes/filter-and-thin/index.html");
  const archive = readFileSync("src/firewall-log-archival.md", "utf8");
  assert.doesNotMatch(
    archive.match(/```limpid\n([\s\S]*?)```/)[1],
    /\bdrop\b|filter_noise/,
  );
  assert.match(archive, /default \{ output other \}/);
});

test("Datadog is fifth and preserves its header and JSON payload", () => {
  const recipes = pages().filter((page) => page.kind === "recipe");
  assert.equal(recipes[4].route, "recipes/datadog/index.html");
  assert.equal(recipes[5].route, "recipes/cloudwatch/index.html");
  assert.equal(recipes[5].number, 6);
  assert.deepEqual(
    recipes.map((recipe, index) => recipe.number ?? index + 1),
    [1, 2, 3, 4, 5, 6, 7, 8],
  );
  assert.equal(recipes[6].route, "recipes/ama-forwarding/index.html");
  assert.equal(recipes[7].route, "recipes/cef-to-amp/index.html");
  const source = readFileSync("src/datadog.md", "utf8");
  for (const text of [
    '"DD-API-KEY":',
    "<DATADOG_API_KEY>",
    "/api/v2/logs",
    "batch_size 1",
    "message: ingress",
    "0.8.4",
    "Log Explorer",
  ])
    assert.ok(source.includes(text), text);
  const payload = source.match(/```limpid\n([\s\S]*?)```/)[1];
  const rendered = recipes[4].content
    .match(/<pre><code[^>]*>([\s\S]*?)<\/code>/)[1]
    .replace(/<span class="[^"]+">|<\/span>/g, "");
  assert.equal(rendered, new MarkdownIt().utils.escapeHtml(payload));
});

test("archival and filtering recipes preserve every authored fence payload", () => {
  for (const name of ["firewall-log-archival", "filter-and-thin"]) {
    const entry = pages().find(
      (page) => page.route === `recipes/${name}/index.html`,
    );
    const source = readFileSync(`src/${name}.md`, "utf8");
    const payloads = [...source.matchAll(/```limpid\n([\s\S]*?)```/g)].map(
      (m) => m[1],
    );
    const rendered = [
      ...entry.content.matchAll(/<pre><code[^>]*>([\s\S]*?)<\/code>/g),
    ].map((m) => m[1].replace(/<span class="[^"]+">|<\/span>/g, ""));
    assert.deepEqual(
      rendered,
      payloads.map((p) => new MarkdownIt().utils.escapeHtml(p)),
    );
  }
});

test("AMA recipe pairs PRI rewriting with distinct connector DCRs", () => {
  const entry = pages().find(
    (page) => page.route === "recipes/ama-forwarding/index.html",
  );
  const html = new PageTemplate().render({ entry });
  assert.equal(entry.title, "Route CEF and Syslog to Log Analytics via AMA");
  for (const text of [
    "local0 only",
    "local1 only",
    "LOG_INFO",
    "CommonSecurityLog",
    "syslog.set_pri(egress, 16, 6)",
    "syslog.set_pri(egress, 17, 6)",
    "connect-cef-syslog-ama",
  ]) {
    assert.ok(
      html.replace(/<span class="[^"]+">|<\/span>/g, "").includes(text),
      text,
    );
  }
  assert.ok(!html.includes("Imported from the existing pipeline examples"));
});

test("Loki is third and distinguishes native JSON from OTLP", () => {
  const recipes = pages().filter((p) => p.kind === "recipe");
  assert.equal(recipes[2].route, "recipes/loki-http-json/index.html");
  const source = readFileSync("src/loki-http-json.md", "utf8");
  for (const text of [
    "batch_size 1",
    "application/json",
    "/loki/api/v1/push",
    '"${to_int(received_at)}"',
  ])
    assert.ok(source.includes(text), text);
  assert.ok(source.includes('stream: { job: "syslog" }'));
  assert.ok(source.includes('values: [["${to_int(received_at)}", ingress]]'));
  for (const text of [
    "/otlp/v1/logs",
    "type otlp_http",
    'protocol "http_protobuf"',
    'key: "service.name"',
    'key: "source.ip"',
    "allow_structured_metadata: true",
    "ignore_defaults: true",
    "action: index_label",
    '{service_name="syslog-forwarder"}',
  ])
    assert.ok(source.includes(text), text);
});

test("Elastic is fourth and documents both ingestion paths and acknowledgement limits", () => {
  const recipes = pages().filter((p) => p.kind === "recipe");
  assert.equal(recipes[3].route, "recipes/elasticsearch/index.html");
  const source = readFileSync("src/elasticsearch.md", "utf8");
  for (const text of [
    "application/x-ndjson",
    "/limpid-syslog/_bulk",
    "batch_size 1",
    "/_otlp/v1/logs",
    "protocol http_protobuf",
    "logs-generic.otel-default",
    "body.text",
    "errors: true",
    "Kibana",
    "Configure Elasticsearch",
  ])
    assert.ok(source.includes(text), text);
  assert.ok(source.includes('to_json({ index: {} }) + "\\n"'));
  assert.doesNotMatch(
    source,
    /docker run|What was verified|0\.7\.8|execution certification/,
  );
});

test("AMP recipe pairs OTLP attributes with Log Analytics record mappings", () => {
  const entry = pages().find(
    (page) => page.route === "recipes/cef-to-amp/index.html",
  );
  assert.ok(entry);
  assert.equal(entry.title, "Map CEF fields to Log Analytics columns via AMP");
  const source = readFileSync("src/cef-to-amp.md", "utf8");
  const mapping = JSON.parse(source.match(/```json\n([\s\S]*?)```/)[1]);
  for (const { from, to } of mapping.recordMap) {
    if (from.startsWith("attributes.")) {
      assert.equal(from, `attributes.${to}`);
      assert.ok(source.includes(`key: "${to}"`), to);
    }
  }
  assert.ok(
    mapping.recordMap.some(
      (m) => m.from === "time_unix_nano" && m.to === "TimeGenerated",
    ),
  );
  assert.ok(source.includes("compose_otlp | otlp_to_egress"));
  assert.ok(source.includes("not a complete AMP deployment template"));
  const directory = new PageTemplate().render({
    entry: pages().find((p) => p.kind === "recipes"),
  });
  assert.ok(directory.includes(url(entry.route)));
  assert.ok(directory.includes(entry.title));
});

test("AMP receiver, processor and DCR form one consistent public configuration", () => {
  const source = readFileSync("src/cef-to-amp.md", "utf8");
  const blocks = [...source.matchAll(/```json\n([\s\S]*?)```/g)].map((m) =>
    JSON.parse(m[1]),
  );
  const amp = blocks.find((b) => b.properties?.receivers)?.properties;
  const dcr = blocks.find((b) => b.properties?.dataFlows)?.properties;
  assert.ok(amp && dcr);
  const pipeline = amp.service.pipelines[0];
  assert.deepEqual(
    pipeline.processors,
    amp.processors.map((p) => p.name),
  );
  assert.equal(amp.processors[0].type, "MicrosoftCommonSecurityLog");
  assert.equal(pipeline.receivers[0], amp.receivers[0].name);
  assert.equal(pipeline.exporters[0], amp.exporters[0].name);
  assert.equal(
    amp.receivers[0].tlsConfiguration,
    amp.tlsConfigurations[0].name,
  );
  assert.equal(amp.tlsConfigurations[0].mode, "serverOnly");
  assert.equal(
    amp.exporters[0].azureMonitorWorkspaceLogs.api.stream,
    dcr.dataFlows[0].streams[0],
  );
  assert.equal(dcr.dataFlows[0].outputStream, "Microsoft-CommonSecurityLog");
  assert.equal(
    dcr.dataFlows[0].destinations[0],
    dcr.destinations.logAnalytics[0].name,
  );
  assert.equal(dcr.dataFlows[0].transformKql, "source");
  assert.ok(source.includes("passthrough: true"));
  assert.ok(source.includes("Monitoring Metrics Publisher"));
  assert.doesNotMatch(
    source,
    /[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}/i,
  );
  assert.doesNotMatch(
    source,
    /\b(?:10\.|172\.(?:1[6-9]|2\d|3[01])\.|192\.168\.)\d/,
  );
});

test("docs directory shows Configuration parents without hiding sidebar children", () => {
  const entries = pages();
  const entry = entries.find((page) => page.kind === "index");
  const template = new PageTemplate();
  const html = template.render({ entry });
  const sections = [
    ...html.matchAll(/<section><h2>([^<]+)<\/h2>([\s\S]*?)<\/section>/g),
  ];
  assert.deepEqual(
    sections.map((match) => match[1]),
    [...new Set(entry.nav.map((item) => item.group))],
  );
  for (const [, group, body] of sections) {
    const expected = entry.nav.filter(
      (item) =>
        item.group === group && (group !== "Configuration" || !item.nested),
    );
    assert.deepEqual(
      [...body.matchAll(/href="([^"]+)"/g)].map((match) => match[1]),
      expected.map((item) => url(item.route)),
      group,
    );
  }
  const children = entry.nav.filter(
    (item) => item.group === "Configuration" && item.nested,
  );
  assert.ok(children.length > 0);
  const article = template.render({
    entry: entries.find((page) => page.kind === "docs"),
  });
  for (const item of children)
    assert.ok(article.includes(`href="${url(item.route)}"`));
  assert.equal(entry.nav.length, 48);
});

test("every existing chapter is rendered once, without an authored content copy", () => {
  const files = readdirSync("../docs/src", { recursive: true }).filter(
    (x) => x.endsWith(".md") && x !== "SUMMARY.md",
  );
  const docs = pages().filter((x) => x.kind === "docs");
  assert.equal(docs.length, 48);
  assert.deepEqual(docs.map((x) => x.file).sort(), files.sort());
  assert.equal(new Set(pages().map((x) => x.route)).size, pages().length);
});

test("navigation links, README routes and repeated headings retain usable targets", () => {
  assert.equal(route("pipelines/README.md"), "docs/pipelines/index.html");
  assert.equal(navigation().length, 48);
  const result = markdown(
    "# Hello\n\n## Hello\n\n[Route](../pipelines/README.md#basic-structure)",
    "operations/cli.md",
  );
  assert.match(result, /id="hello"/);
  assert.match(result, /id="hello-1"/);
  assert.ok(
    result.includes(
      `href="${url("docs/pipelines/index.html#basic-structure")}"`,
    ),
  );
});

test("DSL fence payload is escaped as text, never interpreted as a template", () => {
  const code = 'def process example { egress = "<tag> & ${source.ip}" }';
  const result = markdown("```limpid\n" + code + "\n```");
  assert.ok(
    result
      .replace(/<span class="[^"]+">|<\/span>/g, "")
      .includes("&lt;tag&gt; &amp; ${source.ip}"),
  );
  assert.ok(!result.includes("<tag>"));
  assert.ok(result.includes('class="language-limpid"'));
});

test("published version and pack references use the same release boundary", () => {
  for (const name of [
    "limpid",
    "limpidctl",
    "limpid-prometheus",
    "limpid-metrics-schema",
  ]) {
    assert.match(
      readFileSync(`../crates/${name}/Cargo.toml`, "utf8"),
      /\nversion = "0\.8\.4"/,
    );
  }
  assert.ok(
    markdown(
      "[Pack](https://github.com/naoto256/limpid/blob/main/packaging/snippets/README.md)",
    ).includes("/blob/v0.8.4/"),
  );
});

test("all source fence payloads survive rendering unchanged", () => {
  const parser = new MarkdownIt();
  for (const chapter of pages().filter((x) => x.kind === "docs")) {
    const source = readFileSync(`../docs/src/${chapter.file}`, "utf8");
    const fences = parser
      .parse(source, {})
      .filter((t) => t.type === "fence" || t.type === "code_block");
    const rendered = [
      ...chapter.content.matchAll(/<pre><code[^>]*>([\s\S]*?)<\/code><\/pre>/g),
    ].map((m) => m[1]);
    assert.equal(rendered.length, fences.length, chapter.file);
    fences.forEach((fence, i) =>
      assert.equal(
        rendered[i]
          .replace(/<span class="[^"]+">|<\/span>/g, "")
          .replace(/&#x27;/g, "'"),
        parser.utils.escapeHtml(fence.content),
        chapter.file,
      ),
    );
  }
});
