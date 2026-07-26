// @ts-check
import { defineConfig } from 'astro/config';
import mdx from '@astrojs/mdx';
import sitemap from '@astrojs/sitemap';
import { rehypeBaseLinks } from './src/lib/rehype-base-links.mjs';

/**
 * The site is deployed to more than one target, and they disagree about where
 * the root is: GitHub Pages serves a project site under `/<repo>`, while a
 * Cloudflare Worker serves from `/`. That prefix reaches links in three places —
 * components (`src/lib/url.ts` reads `import.meta.env.BASE_URL`), markdown (the
 * rehype plugin below) and the sitemap — so it is a parameter here rather than a
 * constant anywhere.
 *
 * See `astro.config.cloudflare.mjs` for the root-hosted variant.
 */
/** @param {{ base: string, site: string }} target */
export function makeConfig({ base, site }) {
  return defineConfig({
    site,
    base,
    trailingSlash: 'ignore',
    integrations: [
      mdx(),
      // Match the unslashed form the site links to and the Worker serves, so
      // the sitemap never lists a URL that redirects.
      sitemap({
        serialize: (item) => ({ ...item, url: item.url.replace(/(.)\/$/, '$1') }),
      }),
    ],
    markdown: {
      // Colour is a data channel, not decoration — code is ink on paper, styled
      // by our own stylesheet rather than a syntax-highlighting palette.
      syntaxHighlight: false,
      rehypePlugins: [rehypeBaseLinks(base)],
    },
  });
}

// Default target: GitHub Pages, at https://cldixon.github.io/ursa
export default makeConfig({
  base: '/ursa',
  site: 'https://cldixon.github.io',
});
