// `cf build` packages static assets with Wrangler's bundler internally; this is
// that bundler's setting, not a Wrangler CLI config. Deploys use `cf` only.
import { defineWranglerConfig } from "wrangler/experimental-config";

export default defineWranglerConfig({
	assetsDirectory: "./dist",
});
