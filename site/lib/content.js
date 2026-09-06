import { readFileSync } from "node:fs";
import path from "node:path";
import MarkdownIt from "markdown-it";
import { highlight } from "./highlight.js";
import { url, repository, sourceRef } from "./config.js";

const read = (file) =>
  readFileSync(new URL(`../../${file}`, import.meta.url), "utf8");
export const escape = (text) =>
  String(text).replace(
    /[&<>"']/g,
    (c) =>
      ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" })[
        c
      ],
  );
export function route(file) {
  return (
    "docs/" +
    file
      .replace(/\.md$/, ".html")
      .replace(/(^|\/)README\.html$/, "$1index.html")
  );
}
export function slug(text) {
  return text
    .toLowerCase()
    .replace(/[^\p{L}\p{N}_\-\s]/gu, "")
    .replace(/\s/g, "-");
}
export function markdown(source, file = "introduction.md") {
  const md = new MarkdownIt({ html: false, linkify: false, highlight });
  const counts = new Map();
  md.renderer.rules.heading_open = (tokens, idx, options, env, renderer) => {
    const plain = tokens[idx + 1].children
      .map((t) =>
        t.type === "image"
          ? t.content
          : t.type.endsWith("_inline") || t.type === "text"
            ? t.content
            : "",
      )
      .join("");
    const key = slug(plain);
    const count = counts.get(key) || 0;
    counts.set(key, count + 1);
    tokens[idx].attrSet("id", key + (count ? `-${count}` : ""));
    return renderer.renderToken(tokens, idx, options);
  };
  function resolve(href) {
    if (href.startsWith(`${repository}/blob/main/`))
      return href.replace("/blob/main/", `/blob/${sourceRef}/`);
    if (href.startsWith("#") || /^(?:[a-z]+:|\/\/)/i.test(href)) return href;
    const [pathname, fragment] = href.split("#");
    const target = path.posix.normalize(
      path.posix.join(path.posix.dirname(file), pathname),
    );
    if (target.startsWith("../"))
      return `${repository}/blob/${sourceRef}/${target.replace(/^(\.\.\/)+/, "")}${fragment ? "#" + fragment : ""}`;
    return (
      url(target.endsWith(".md") ? route(target) : `docs/${target}`) +
      (fragment ? "#" + fragment : "")
    );
  }
  md.renderer.rules.link_open = (tokens, idx, options, env, renderer) => {
    tokens[idx].attrSet("href", resolve(tokens[idx].attrGet("href")));
    return renderer.renderToken(tokens, idx, options);
  };
  const image = md.renderer.rules.image;
  md.renderer.rules.image = (tokens, idx, options, env, renderer) => {
    tokens[idx].attrSet("src", resolve(tokens[idx].attrGet("src")));
    tokens[idx].attrSet("loading", "lazy");
    return image(tokens, idx, options, env, renderer);
  };
  return md.render(source);
}

export function navigation() {
  const items = [];
  let group = "Overview";
  for (const line of read("docs/src/SUMMARY.md").split("\n")) {
    if (line.startsWith("# ") && line !== "# Summary") group = line.slice(2);
    const match = line.match(/^(\s*)(?:- )?\[([^\]]+)\]\(\.\/([^)]*)\)/);
    if (match)
      items.push({
        title: match[2],
        file: match[3],
        group,
        nested: match[1].length > 0,
        route: route(match[3]),
      });
  }
  return items;
}
export function pages() {
  for (const name of [
    "limpid",
    "limpidctl",
    "limpid-prometheus",
    "limpid-metrics-schema",
  ]) {
    if (!read(`crates/${name}/Cargo.toml`).includes('\nversion = "0.8.4"\n')) {
      throw new Error(
        "Site targets stable 0.8.4; review content/version before building a different release",
      );
    }
  }
  const nav = navigation();
  const chapters = nav.map((item) => ({
    ...item,
    kind: "docs",
    content: markdown(read(`docs/src/${item.file}`), item.file),
    nav,
  }));
  const examples = read("docs/src/pipelines/examples.md");
  const sections = examples.split(/^## /m).slice(1);
  // Present existing documented configurations with their original scope and caveats.
  const recipes = sections.slice(0, 2).map((section, index) => {
    const recipeSource =
      index === 1
        ? readFileSync(
            new URL("../src/ama-forwarding.md", import.meta.url),
            "utf8",
          )
        : readFileSync(
            new URL("../src/firewall-log-archival.md", import.meta.url),
            "utf8",
          );
    const title =
      index === 1
        ? "Route CEF and Syslog to Log Analytics via AMA"
        : "Archive firewall logs in per-device files";
    return {
      kind: "recipe",
      number: index === 1 ? 6 : 1,
      title,
      description:
        index === 0
          ? "Strip syslog PRI and route firewall logs into per-device archives."
          : "Separate CEF and non-CEF facilities for AMA collection rules, with disk-backed forwarding.",
      route: `recipes/${index === 0 ? "firewall-log-archival" : "ama-forwarding"}/index.html`,
      content: markdown(recipeSource, "pipelines/examples.md"),
      siteRecipe: true,
    };
  });
  recipes.push({
    kind: "recipe",
    title: "Send CEF to Log Analytics via AMP",
    number: 7,
    description:
      "Put CEF fields into OTLP attributes that AMP can map to CommonSecurityLog columns—not just a message body.",
    route: "recipes/cef-to-amp/index.html",
    content: markdown(
      readFileSync(new URL("../src/cef-to-amp.md", import.meta.url), "utf8"),
      "pipelines/examples.md",
    ),
    siteRecipe: true,
  });
  recipes.splice(1, 0, {
    kind: "recipe",
    title: "Send syslog to Loki",
    description:
      "Choose native JSON or OTLP, control label placement, and preserve the original log line.",
    route: "recipes/loki-http-json/index.html",
    content: markdown(
      readFileSync(
        new URL("../src/loki-http-json.md", import.meta.url),
        "utf8",
      ),
      "pipelines/examples.md",
    ),
    siteRecipe: true,
  });
  recipes.splice(2, 0, {
    kind: "recipe",
    title: "Send syslog to Elasticsearch",
    description:
      "Build searchable JSON documents, or let Elasticsearch ingest OTLP logs directly.",
    route: "recipes/elasticsearch/index.html",
    content: markdown(
      readFileSync(new URL("../src/elasticsearch.md", import.meta.url), "utf8"),
      "pipelines/examples.md",
    ),
    siteRecipe: true,
  });
  recipes.splice(3, 0, {
    kind: "recipe",
    title: "Send syslog to Datadog",
    description:
      "Preserve the original line and send searchable context to Datadog with JSON or OTLP.",
    route: "recipes/datadog/index.html",
    content: markdown(
      readFileSync(new URL("../src/datadog.md", import.meta.url), "utf8"),
      "pipelines/examples.md",
    ),
    siteRecipe: true,
  });
  recipes.splice(4, 0, {
    kind: "recipe",
    number: 5,
    title: "Send syslog to Amazon CloudWatch Logs",
    description:
      "Choose JSON fields or OTLP attributes, with direct HTTPS ingestion into an AWS log group.",
    route: "recipes/cloudwatch/index.html",
    content: markdown(
      readFileSync(new URL("../src/cloudwatch.md", import.meta.url), "utf8"),
      "pipelines/examples.md",
    ),
    siteRecipe: true,
  });
  recipes.splice(1, 0, {
    kind: "recipe",
    title: "Drop unwanted logs, or keep the first occurrence",
    description:
      "Discard selected noise, or use a table to suppress repeated attack logs without losing the first occurrence.",
    route: "recipes/filter-and-thin/index.html",
    content: markdown(
      readFileSync(
        new URL("../src/filter-and-thin.md", import.meta.url),
        "utf8",
      ),
      "pipelines/examples.md",
    ),
    siteRecipe: true,
  });
  recipes.splice(5, 0, {
    kind: "recipe",
    title: "Send syslog to Better Stack",
    description:
      "Keep the original log line and add searchable fields with JSON or OpenTelemetry over HTTPS.",
    route: "recipes/better-stack/index.html",
    content: markdown(
      readFileSync(new URL("../src/better-stack.md", import.meta.url), "utf8"),
      "pipelines/examples.md",
    ),
    siteRecipe: true,
  });
  recipes.splice(2, 0, {
    kind: "recipe",
    title: "Archive every log and forward selected events",
    description:
      "Archive every line, forward selected severities, and create a separate structured copy for urgent events.",
    route: "recipes/branch-and-forward/index.html",
    content: markdown(
      readFileSync(
        new URL("../src/branch-and-forward.md", import.meta.url),
        "utf8",
      ),
      "pipelines/examples.md",
    ),
    siteRecipe: true,
  });
  recipes.forEach((recipe, index) => {
    recipe.number = index + 1;
  });
  const readme = read("README.md");
  const code = readme.match(/```limpid\n([\s\S]*?)```/)[1].trimEnd();
  return [
    {
      kind: "home",
      title: "Readable log pipelines",
      route: "index.html",
      code,
    },
    { kind: "index", title: "Documentation", route: "docs/index.html", nav },
    {
      kind: "recipes",
      title: "Recipes",
      route: "recipes/index.html",
      recipes,
    },
    ...chapters,
    ...recipes,
    {
      kind: "example",
      title: "Run the homepage example: FortiGate CEF to OCSF",
      route: "recipes/fortigate-cef-to-ocsf/index.html",
      content: markdown(
        readFileSync(
          new URL("../src/fortigate-cef-to-ocsf.md", import.meta.url),
          "utf8",
        ),
        "pipelines/examples.md",
      ),
      siteRecipe: true,
    },
  ];
}
