// @ts-check
import { makeConfig } from './astro.config.mjs';

/**
 * The Cloudflare Worker target. Served from the root of a workers.dev subdomain
 * or a custom domain, so there is no path prefix.
 *
 * Selected by file rather than by an environment variable so `npm run build:cf`
 * behaves identically on every shell — no `VAR=x cmd` prefix that Windows would
 * not understand.
 *
 * `SITE_URL` only affects canonical tags and the sitemap. Set it once the worker
 * has its real hostname; until then the built site works, its canonical URLs
 * just point at the placeholder.
 */
export default makeConfig({
  base: '/',
  site: process.env.SITE_URL ?? 'https://ursa-docs.workers.dev',
});
