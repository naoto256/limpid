import { canonical } from "../lib/config.js";
import { escape } from "../lib/content.js";

export default class {
  data() {
    return { permalink: "sitemap.xml", eleventyExcludeFromCollections: true };
  }
  render({ pages }) {
    return `<?xml version="1.0" encoding="UTF-8"?>\n<urlset xmlns="http://www.sitemaps.org/schemas/sitemap/0.9">${pages.map((page) => `<url><loc>${escape(canonical(page.route))}</loc></url>`).join("")}</urlset>\n`;
  }
}
