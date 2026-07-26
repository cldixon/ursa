// @ts-check
import { defineConfig } from 'astro/config';
import mdx from '@astrojs/mdx';
import sitemap from '@astrojs/sitemap';
import { rehypeBaseLinks } from './src/lib/rehype-base-links.mjs';

const BASE = '/ursa';

// Published to GitHub Pages under the repository path. Both values are read by
// `Astro.site` / `import.meta.env.BASE_URL`, so every internal link goes through
// `src/lib/url.ts` (components) or the rehype plugin (markdown) rather than
// hardcoding the prefix.
export default defineConfig({
  site: 'https://cldixon.github.io',
  base: BASE,
  trailingSlash: 'ignore',
  integrations: [mdx(), sitemap()],
  markdown: {
    // Colour is a data channel, not decoration — code is ink on paper, styled by
    // our own stylesheet rather than a syntax-highlighting palette.
    syntaxHighlight: false,
    rehypePlugins: [rehypeBaseLinks(BASE)],
  },
});
