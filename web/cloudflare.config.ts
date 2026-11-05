import { bindings, defineConfig } from "cf/config";

export default defineConfig({
	worker: {
		name: "ursa-shell",
		compatibilityDate: "2026-10-01",
		// Every request reaches the Worker first: it serves /data/* (the sample
		// datasets) and /engine/*.wasm (the engine) from R2, and hands every
		// other path to the static assets in dist/.
		entrypoint: "./src/index.ts",
		assets: {
			notFoundHandling: "404-page",
			runWorkerFirst: true,
		},
		env: {
			ASSETS: bindings.assets(),
			DATA: bindings.r2({ name: "ursa-graph" }),
		},
		// For now the shell lives on the account's workers.dev URL; a domain comes
		// later (#279). Uploaded versions also get preview URLs.
		workersDev: true,
		previewUrls: true,
	},
});
