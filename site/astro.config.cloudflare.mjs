// @ts-check
import { makeConfig } from './astro.config.mjs';

/**
 * The Cloudflare Worker target. Served from the root of the workers.dev
 * subdomain or a custom domain, so there is no path prefix.
 *
 * Selected by file rather than by an environment variable so `bun run build:cf`
 * behaves identically on every shell — no `VAR=x cmd` prefix that Windows would
 * not understand.
 *
 * `SITE_URL` only affects canonical tags and the sitemap. The default is the
 * deployed workers.dev hostname; override it when the worker moves to a custom
 * domain.
 */
export default makeConfig({
  base: '/',
  site: process.env.SITE_URL ?? 'https://ursa-docs.cl-dixon.workers.dev',
});
