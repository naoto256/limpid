import { pages } from "./lib/content.js";
import { base } from "./lib/config.js";

export default function (config) {
  config.addGlobalData("pages", pages);
  config.addWatchTarget("../docs/src/");
  config.addWatchTarget("../README.md");
  config.addWatchTarget("../packaging/snippets/");
  config.addPassthroughCopy({
    "src/style.css": "style.css",
    "src/copy-code.js": "copy-code.js",
    "src/mark.svg": "mark.svg",
    "src/fonts": "fonts",
    "../docs/src/assets": "docs/assets",
  });
  return {
    dir: { input: "src", output: "dist" },
    pathPrefix: base,
    templateFormats: ["11ty.js"],
  };
}
