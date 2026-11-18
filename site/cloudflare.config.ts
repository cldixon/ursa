import { defineConfig } from "cf/config";

export default defineConfig({
	worker: {
		name: "ursa-site",
		compatibilityDate: "2026-10-01",
		// No Worker code: the site is static, Astro's build in dist/ (see
		// wrangler.config.ts), served as Workers static assets.
		assets: {
			// Astro writes every page as <path>/index.html and links to <path>/
			// (trailingSlash: "always"), so /docs redirects to /docs/.
			htmlHandling: "force-trailing-slash",
			// Starlight's 404 page, dist/404.html.
			notFoundHandling: "404-page",
		},
		// The site's home is https://ursa.cldixon.dev. Attach the domain in the
		// Cloudflare dashboard (see README.md), or list it here to have deploys
		// apply it: domains: ["ursa.cldixon.dev"].
		// Until then, the site is on the account's workers.dev URL; uploaded
		// versions also get preview URLs.
		workersDev: true,
		previewUrls: true,
	},
});
