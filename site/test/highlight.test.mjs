import test from "node:test";
import assert from "node:assert/strict";
import { highlight } from "../lib/highlight.js";
import { markdown } from "../lib/content.js";
import { readFileSync, readdirSync } from "node:fs";
import { configurationKeys } from "../lib/configuration-keys.js";

test("locals and lambda parameters are not configuration keys", () => {
  const code =
    'let headers = map(workspace.headers) { |key, value| "${key}: ${value}" }\nlet byte_total = reduce(workspace.fields, 0) { |acc, key, value| acc + value }';
  for (const source of [
    code,
    `def process example { ${code} }`,
    `def function example(fields) { ${code} }`,
  ]) {
    const html = highlight(source, "limpid");
    assert.ok(!html.includes("hljs-attr"));
    assert.match(html, /hljs-title function_">map</);
    assert.match(html, /hljs-title function_">reduce</);
  }
});

test("every production PropertySpec key is covered by the highlighting inventory", () => {
  const root = new URL("../../crates/limpid/src/", import.meta.url);
  const names = new Set();
  for (const file of readdirSync(root, { recursive: true }).filter((file) =>
    file.endsWith(".rs"),
  )) {
    const source = readFileSync(new URL(file, root), "utf8")
      .split(/#\[cfg\(test\)\]\s*mod tests/)[0]
      .replace(/\/\/[^\n]*/g, "");
    for (const match of source.matchAll(
      /PropertySpec\s*\{\s*name:\s*"([a-z_][a-z_0-9]*)"/g,
    ))
      names.add(match[1]);
  }
  assert.ok(names.size > 50);
  assert.deepEqual(configurationKeys, [...names].sort());
  for (const key of names) {
    const scope = "attr";
    assert.ok(
      highlight("def input example { " + key + " 1 }", "limpid").includes(
        `class="hljs-${scope}">${key}</span>`,
      ),
      key,
    );
  }
});

test("node directives and peer settings have distinct scopes", () => {
  const html = highlight(
    'node_id "host01"\nnode_key "/etc/limpid/node.key"\ndef input receiver { peer { node_id "host02" pubkey "example" } }',
    "limpid",
  );
  assert.match(html, /hljs-keyword">node_id</);
  assert.match(html, /hljs-keyword">node_key</);
  assert.match(html, /hljs-attr">node_id</);
});

test("limpid uses standard scopes without coloring keywords inside strings or comments", () => {
  const html = highlight(
    'def process p { egress = "if 42" // drop\n syslog.set_pri(egress, 16, 6) }',
    "limpid",
  );
  assert.match(html, /hljs-keyword">def</);
  assert.match(html, /hljs-string">&quot;if 42&quot;</);
  assert.match(html, /hljs-comment">\/\/ drop</);
  assert.match(html, /hljs-number">16</);
  assert.match(html, /hljs-title function_">syslog.set_pri</);
});

test("configuration keys and braces use subtle scopes outside strings and comments", () => {
  assert.match(
    highlight("def input example { rate_limit 1000 }", "limpid"),
    /hljs-attr">rate_limit</,
  );
  const html = highlight(
    'def output example { peer { host "type {}" port 28330 } } // bind {}',
    "limpid",
  );
  assert.match(html, /hljs-attr">peer</);
  assert.match(html, /hljs-attr">host</);
  assert.match(html, /hljs-attr">port</);
  assert.equal((html.match(/hljs-punctuation/g) || []).length, 4);
  assert.match(html, /hljs-string">&quot;type \{\}&quot;</);
  assert.match(html, /hljs-comment">\/\/ bind \{\}</);
});

test("field paths are plain while dotted calls and standalone keys retain their scopes", () => {
  const html = highlight(
    'workspace.host workspace.tls.cert source.port local.path\ndef output example { peer { host "localhost" } }\nsyslog.set_pri(egress, 16, 6)',
    "limpid",
  );
  for (const path of [
    "workspace.host",
    "workspace.tls.cert",
    "source.port",
    "local.path",
  ])
    assert.ok(html.includes(path));
  assert.equal((html.match(/hljs-attr/g) || []).length, 2);
  assert.match(html, /hljs-title function_">syslog.set_pri</);
});

test("interpolation handles inner quotes, field paths and nested blocks without losing bytes", () => {
  const code =
    'path "/var/log/limpid/${source.ip}/${strftime(received_at, "%Y-%m-%d", "local")}.log"';
  const html = highlight(code, "limpid");
  assert.match(html, /hljs-title function_">strftime</);
  assert.ok(html.includes("source.ip"));
  assert.match(html, /hljs-string">&quot;%Y-%m-%d&quot;</);
  assert.ok(html.includes("}.log") || html.includes("</span></span>.log"));
  const plain = html.replace(/<span class="[^"]+">|<\/span>/g, "");
  assert.equal(plain, code.replaceAll('"', "&quot;"));
  const nested = highlight(
    '"${switch x { 1 { "yes" } default { "no" } }} tail"',
    "limpid",
  );
  assert.match(nested, /hljs-string">&quot;yes&quot;</);
  assert.ok(nested.endsWith(" tail&quot;</span>"));
  assert.ok(!highlight('"\\${source.ip}"', "limpid").includes("hljs-subst"));
});

test("pipe targets use the function scope with or without parentheses", () => {
  const html = highlight(
    'workspace.users = workspace.events\n |> filter { |e| e.type == "auth" }\n |> map { |e| e.user }\n |> distinct',
    "limpid",
  );
  for (const name of ["filter", "map", "distinct"])
    assert.ok(html.includes(`class="hljs-title function_">${name}</span>`));
  assert.ok(html.includes("e.type"));
});

test("process chains highlight named stages without confusing lambda parameters", () => {
  const html = highlight(
    "process strip_headers | enrich | {\nworkspace.geo = geoip(workspace.src)\negress = to_json(workspace)\n}",
    "limpid",
  );
  for (const name of ["strip_headers", "enrich", "geoip", "to_json"])
    assert.ok(
      html.includes(`class="hljs-title function_">${name}</span>`),
      name,
    );
  const lambda = highlight(
    "map(workspace.headers) { |key, value| key }",
    "limpid",
  );
  assert.ok(!lambda.includes('function_">key'));
  assert.ok(!lambda.includes('function_">value'));
});

test("multiline process chains continue after inline blocks and stop before output", () => {
  const html = highlight(
    "def pipeline p {\nprocess parse_x\n | x_to_otlp\n | compose_ocsf\n | { workspace.lsis.shed.otlp.log_record.body =\n { string_value: workspace.lsis.composed.ocsf } }\n | compose_otlp\n | otlp_to_egress\noutput sink\n}",
    "limpid",
  );
  for (const name of [
    "parse_x",
    "x_to_otlp",
    "compose_ocsf",
    "compose_otlp",
    "otlp_to_egress",
  ])
    assert.ok(
      html.includes(`class="hljs-title function_">${name}</span>`),
      name,
    );
  assert.match(html, /hljs-keyword">output</);
  assert.ok(!html.includes('function_">string_value'));
  assert.ok(!html.includes('function_">sink'));
});

test("other languages share scopes; unknown and text fences remain escaped plain text", () => {
  assert.match(highlight('{"port": 28330}', "json"), /hljs-number/);
  assert.match(highlight('echo "hello"', "bash"), /hljs-string/);
  for (const language of ["text", "unknown-language", ""]) {
    const html = markdown(
      "```" + language + "\n<script>alert(1)</script>\n```",
    );
    assert.ok(!html.includes("<script>"));
    assert.ok(!html.includes("hljs-"));
    assert.ok(html.includes("&lt;script&gt;"));
  }
});
