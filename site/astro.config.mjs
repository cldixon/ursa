// @ts-check
import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { defineConfig } from 'astro/config';
import mdx from '@astrojs/mdx';
import sitemap from '@astrojs/sitemap';

// The release the site documents, read from the workspace `Cargo.toml` — the
// same single source the Python distribution takes `__version__` from. Resolved
// here rather than in a module under `src/`: this config is not bundled, so its
// `import.meta.url` still points at the repository, while a bundled module's
// would point into `dist/` and the read would miss.
//
// Injected as a compile-time constant so nothing touches the filesystem at
// render time and nothing ships to the browser.
const version = (() => {
  const cargo = fileURLToPath(new URL('../Cargo.toml', import.meta.url));
  // Scoped to [workspace.package]: several tables carry a `version` key, so
  // matching the first one anywhere would pick up whichever sorts first.
  const table = readFileSync(cargo, 'utf8').split(/^\[workspace\.package\]\s*$/m)[1];
  const match = table?.split(/^\[/m)[0].match(/^\s*version\s*=\s*"([^"]+)"/m);
  if (!match) throw new Error('no [workspace.package] version in Cargo.toml');
  return match[1];
})();

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
  vite: {
    define: { __URSA_VERSION__: JSON.stringify(version) },
  },
});
