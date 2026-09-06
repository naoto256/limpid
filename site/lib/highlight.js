import hljs from "highlight.js/lib/core";
import { configurationKeys } from "./configuration-keys.js";
import bash from "highlight.js/lib/languages/bash";
import ini from "highlight.js/lib/languages/ini";
import json from "highlight.js/lib/languages/json";
import yaml from "highlight.js/lib/languages/yaml";
import protobuf from "highlight.js/lib/languages/protobuf";

// Standard Highlight.js scopes keep the DSL on the same theme as other languages.
// This is presentation, not validation; the Rust DSL grammar remains authoritative.
hljs.registerLanguage("limpid", (h) => {
  const keywords = {
    keyword:
      "def input output process pipeline function include node_id node_key let if else switch default try catch drop finish error and or not",
    literal: "true false null",
    variable: "ingress egress workspace source",
  };
  const string = { scope: "string", begin: /"/, end: /"/, contains: [] };
  const interpolation = {
    scope: "subst",
    begin: /\$\{/,
    end: /\}/,
    beginScope: "punctuation",
    endScope: "punctuation",
    keywords,
    contains: [],
  };
  const processChain = {
    begin: /\bprocess\b/,
    beginScope: "keyword",
    end: /(?=[;}]|\r?\n(?!\s*\|))/,
    contains: [],
  };
  const expressions = [
    h.C_LINE_COMMENT_MODE,
    string,
    processChain,
    h.C_NUMBER_MODE,
    {
      match: [/\|>/, /\s*/, /[A-Za-z_]\w*(?:\.[A-Za-z_]\w*)*/],
      scope: { 1: "operator", 3: "title.function" },
    },
    {
      scope: "title.function",
      match: /\b[A-Za-z_][\w]*(?:\.[A-Za-z_][\w]*)*(?=\s*\()/,
    },
    // Consume field paths as a unit before standalone keyword classification.
    // Function calls above take precedence, including namespaced primitives.
    { match: /\b[A-Za-z_]\w*(?:\.[A-Za-z_]\w*)+/, relevance: 0 },
  ];
  // Nested blocks inside an interpolation must not close the outer ${...}.
  const block = {
    begin: /\{/,
    end: /\}/,
    beginScope: "punctuation",
    endScope: "punctuation",
    keywords,
    contains: [...expressions, "self"],
  };
  string.contains = [h.BACKSLASH_ESCAPE, interpolation];
  processChain.contains = [
    h.C_LINE_COMMENT_MODE,
    block,
    { scope: "title.function", match: /\b[A-Za-z_]\w*/ },
    { scope: "operator", match: /\|/ },
  ];
  interpolation.contains = [...expressions, block];
  const settingsKeywords = {
    ...keywords,
    keyword: keywords.keyword
      .split(" ")
      .filter((word) => word !== "node_id")
      .join(" "),
    attr: configurationKeys.join(" "),
  };
  const settingsBlock = {
    begin: /\{/,
    end: /\}/,
    beginScope: "punctuation",
    endScope: "punctuation",
    keywords: settingsKeywords,
    contains: [...expressions, "self"],
  };
  // Only declaration/global settings bodies enable property-name scopes.
  // Expression bodies and string interpolations retain expression-only keywords.
  const settingsDefinition = {
    begin: [
      /\bdef/,
      /\s+/,
      /(?:input|output)\b/,
      /\s+/,
      /[A-Za-z_]\w*/,
      /\s*/,
      /\{/,
    ],
    beginScope: { 1: "keyword", 3: "keyword", 7: "punctuation" },
    end: /\}/,
    endScope: "punctuation",
    keywords: settingsKeywords,
    contains: [...expressions, settingsBlock],
  };
  const globalSettings = {
    begin: [/^\s*\b(?:control|geoip|table|tls)\b/, /\s*/, /\{/],
    beginScope: { 1: "attr", 3: "punctuation" },
    end: /\}/,
    endScope: "punctuation",
    keywords: settingsKeywords,
    contains: [...expressions, settingsBlock],
  };
  return {
    name: "limpid",
    keywords,
    contains: [settingsDefinition, globalSettings, ...expressions, block],
  };
});
for (const [name, definition] of Object.entries({
  bash,
  ini,
  json,
  yaml,
  protobuf,
})) {
  hljs.registerLanguage(name, definition);
}
hljs.registerAliases("jsonc", { languageName: "json" });

export function highlight(code, language) {
  // Never guess a language for plain text or unsupported fences.
  return hljs.getLanguage(language)
    ? hljs.highlight(code, { language, ignoreIllegals: true }).value
    : "";
}
