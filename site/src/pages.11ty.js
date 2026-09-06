import { escape } from "../lib/content.js";
import { highlight } from "../lib/highlight.js";
import {
  url,
  release,
  repository,
  contentRef,
  canonical,
} from "../lib/config.js";

function nav(items, current) {
  let group;
  return items
    .map((item) => {
      const heading =
        group === item.group ? "" : `<h3>${escape(item.group)}</h3>`;
      group = item.group;
      return `${heading}<a ${item.route === current ? 'aria-current="page"' : ""} class="${item.nested ? "nested" : ""}" href="${url(item.route)}">${escape(item.title)}</a>`;
    })
    .join("");
}
function content(page) {
  if (page.kind === "home")
    return `<main id="main" class="home">
    <section class="hero">
    <h1>Log pipelines,<br><span>limpid as intent.</span></h1>
    <ul class="hero-questions">
      <li>Found out what your pipeline dropped only because the destination's dashboard went quiet?</li>
      <li>Paged at 3 a.m. because a config typo crashed the daemon — and there's no rollback?</li>
      <li>Waiting weeks on a plugin release because a vendor added a field?</li>
    </ul>
    <p class="hero-answer">limpid is for you.</p>
    <div class="hero-bottom"><p>It is a log pipeline daemon where most of the work is <em>picking which pieces to use</em>.</p><div class="actions"><a class="button" href="${url("docs/getting-started.html")}">Start with the docs <span>→</span></a></div></div></section>
    <section class="pipeline-surface" aria-labelledby="flow-title"><div class="pipeline-intent"><h2 id="flow-title">What you want to do</h2><div class="flow"><div><p>Receive the wire</p><code>fortigate_syslog</code></div><span class="arrow" aria-hidden="true">→</span><div><p>Parse. Shape. Compose.</p><code>parse_syslog | … | ocsf_to_egress</code></div><span class="arrow" aria-hidden="true">→</span><div><p>Send the result</p><code>security_lake</code></div></div></div><div class="code-label"><h3>What you write</h3><span>pipeline.limpid</span></div><pre><code>${highlight(page.code, "limpid")}</code></pre><p class="code-note">Pipeline fragment from the project README, v${release}. Input, output, and snippet definitions are configured separately. <a href="${url("docs/snippets/index.html")}">Meet the snippet library →</a></p></section>
    <section class="principles"><div><span class="number">01</span><h2>Compose small processes.</h2><p>Chain named parsers and composers. Keep vendor-specific logic in editable <code>.limpid</code> snippets.</p></div><div><span class="number">02</span><h2>See the event in flight.</h2><p>Inspect input, process, and output events with the built-in debug tap.</p><a href="${url("docs/operations/tap.html")}">Inspect a pipeline →</a></div><div><span class="number">03</span><h2>Make recovery explicit.</h2><p>Configure disk-backed output queues and an error log for events that need attention.</p><a href="${url("docs/operations/error-log.html")}">Read the recovery contract →</a></div></section></main>`;
  if (page.kind === "index")
    return `<main id="main" class="landing"><div class="eyebrow">DOCUMENTATION / STABLE ${release}</div><h1>From first event<br>to everyday operation.</h1><p class="lede">The language, transports, and operational contracts of limpid.</p><div class="doc-directory">${[
      ...new Set(page.nav.map((x) => x.group)),
    ]
      .map(
        (group) =>
          `<section><h2>${escape(group)}</h2>${page.nav
            .filter(
              (x) =>
                x.group === group && (group !== "Configuration" || !x.nested),
            )
            .map(
              (x) =>
                `<a href="${url(x.route)}">${escape(x.title)} <span>→</span></a>`,
            )
            .join("")}</section>`,
      )
      .join("")}</div></main>`;
  if (page.kind === "recipes")
    return `<main id="main" class="landing"><div class="eyebrow">RECIPES / STABLE ${release}</div><h1>Start with<br>a concrete problem.</h1><p class="lede">Configuration recipes for concrete logging problems. Read the assumptions, then adapt the inputs and destinations to your environment.</p><div class="recipe-list">${page.recipes.map((r, i) => `<a href="${url(r.route)}"><span class="number">0${r.number ?? i + 1}</span><div><span class="eyebrow">DOCUMENTED CONFIGURATION</span><h2>${escape(r.title)}</h2><p>${escape(r.description)}</p></div><span>→</span></a>`).join("")}</div><p>Looking for language semantics? <a href="${url("docs/pipelines/index.html")}">Pipeline reference →</a></p></main>`;
  const article = `<article class="prose"><div class="eyebrow">${page.kind === "recipe" ? "DOCUMENTED CONFIGURATION" : "DOCUMENTATION"} / STABLE ${release}</div>${page.kind === "recipe" && !page.siteRecipe ? `<p class="notice">Imported from the existing pipeline examples. Environment-specific endpoints and paths require configuration; this page does not establish a new integration certification.</p>` : ""}${page.content}${page.siteRecipe ? "" : `<div class="article-source"><a href="${repository}/blob/${contentRef}/docs/src/${page.file || "pipelines/examples.md"}">Read the source on GitHub ↗</a></div>`}</article>`;
  return page.kind === "docs"
    ? `<main id="main" class="docs-layout"><aside><details open><summary>Documentation</summary><nav aria-label="Documentation">${nav(page.nav, page.route)}</nav></details></aside>${article}</main>`
    : `<main id="main" class="recipe-detail"><a href="${url("recipes/")}">← All recipes</a>${article}</main>`;
}
export default class {
  data() {
    return {
      pagination: { data: "pages", size: 1, alias: "entry" },
      permalink: (data) => data.entry.route,
    };
  }
  render({ entry }) {
    return `<!doctype html><html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width, initial-scale=1"><title>${escape(entry.title)} — limpid</title><link rel="canonical" href="${escape(canonical(entry.route))}"><meta name="description" content="Readable log pipelines. Documentation and practical configurations for limpid ${release}."><link rel="stylesheet" href="${url("style.css")}"><link rel="icon" type="image/svg+xml" href="${url("mark.svg")}"><script type="module" src="${url("copy-code.js")}"></script></head><body><a class="skip" href="#main">Skip to content</a><header><a class="brand" href="${url()}"><img class="brand-mark" src="${url("mark.svg")}" width="42.5" height="37" alt="">limpid<span class="version">v${release}</span></a><nav aria-label="Main"><a href="${url("docs/")}">Docs</a><a href="${url("recipes/")}">Recipes</a><a class="github" href="${repository}">GitHub ↗</a></nav></header>${content(entry)}<footer><a class="brand" href="${url()}">limpid</a><p>Log pipelines, limpid as intent.</p><a href="${repository}/releases/tag/v${release}">Stable ${release} ↗</a><span>MIT / Apache-2.0</span></footer></body></html>`;
  }
}
