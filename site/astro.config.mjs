// @ts-check
import { defineConfig } from 'astro/config';
import mdx from '@astrojs/mdx';
import sitemap from '@astrojs/sitemap';

// One deploy target: a Cloudflare Worker serving static assets from the root,
// so there is no base-path prefix anywhere. `SITE_URL` only affects canonical
// tags and the sitemap; the default is the custom domain the worker serves.
export default defineConfig({
  site: process.env.SITE_URL ?? 'https://ursa.cldixon.dev',
  base: '/',
  trailingSlash: 'ignore',
  integrations: [
    mdx(),
    // Match the unslashed form the site links to and the Worker serves
    // (html_handling: drop-trailing-slash), so the sitemap never lists a URL
    // that redirects.
    sitemap({
      serialize: (item) => ({ ...item, url: item.url.replace(/(.)\/$/, '$1') }),
    }),
  ],
  markdown: {
    // Colour is a data channel, not decoration — code is ink on paper, styled by
    // our own stylesheet rather than a syntax-highlighting palette.
    syntaxHighlight: false,
  },
});
