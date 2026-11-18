// The Ursa website: the landing page at / (src/pages/index.astro) and the docs
// at /docs/ (Starlight). Starlight serves only the /docs/ tree: its pages live
// in src/content/docs/docs/, so each one's slug starts with docs/.
import starlight from "@astrojs/starlight";
import { defineConfig } from "astro/config";
import starlightLinksValidator from "starlight-links-validator";

export default defineConfig({
	site: "https://ursa.cldixon.dev",
	trailingSlash: "always",
	integrations: [
		starlight({
			title: "Ursa Docs",
			description: "Graph analytics in SQL: a SQL engine and a DuckDB-like CLI over Parquet, CSV and JSON.",
			favicon: "/favicon.svg",
			// A broken link between docs pages fails the build.
			plugins: [
				starlightLinksValidator({
					// The landing page is an Astro page, not a Starlight one.
					exclude: ["/"],
				}),
			],
			social: [
				{ icon: "github", label: "GitHub", href: "https://github.com/cldixon/ursa" },
			],
			editLink: {
				baseUrl: "https://github.com/cldixon/ursa/edit/main/site/",
			},
			expressiveCode: {
				themes: ["github-dark", "github-light"],
				// Plain frames for shell snippets too: no terminal window chrome.
				defaultProps: { frame: "code" },
				styleOverrides: {
					borderRadius: "0.5rem",
					// The page's own warm greys (docs.css: --sl-color-gray-6 dark,
					// --sl-color-gray-7 light), not GitHub's cooler backgrounds.
					codeBackground: ({ theme }) => (theme.type === "dark" ? "#1c1c19" : "#f8f7f4"),
					borderColor: ({ theme }) => (theme.type === "dark" ? "#34332f" : "#dcd9d2"),
					codeFontFamily: "var(--ursa-font-mono)",
					codeFontSize: "0.86rem",
					codeLineHeight: "1.55",
					frames: { frameBoxShadowCssValue: "none" },
				},
			},
			customCss: ["./src/styles/fonts.css", "./src/styles/docs.css"],
			components: {
				Header: "./src/components/DocsHeader.astro",
				SiteTitle: "./src/components/SiteTitle.astro",
				MobileMenuFooter: "./src/components/MobileMenuFooter.astro",
				// One-click light/dark toggle in place of the Auto/Light/Dark select.
				ThemeSelect: "./src/components/ThemeToggle.astro",
			},
			sidebar: [
				{
					label: "Get started",
					items: [
						{ label: "Overview", slug: "docs" },
						{ label: "Installation", slug: "docs/installation" },
						{ label: "Quickstart", slug: "docs/quickstart" },
						{ label: "How-to Guides", slug: "docs/guides" },
					],
				},
				{
					label: "Topics",
					items: [
						{
							label: "Import & Export",
							collapsed: true,
							items: [
								{ label: "Files", slug: "docs/import-export/files" },
								{ label: "Object storage", slug: "docs/import-export/object-storage" },
								{ label: "Output formats", slug: "docs/import-export/output" },
							],
						},
						{
							label: "SQL",
							collapsed: true,
							items: [
								{ label: "Introduction", slug: "docs/sql/introduction" },
								{ label: "Statements", slug: "docs/sql/statements" },
							],
						},
						{
							label: "Graphs",
							collapsed: true,
							items: [
								{ label: "Property graphs", slug: "docs/graphs/property-graphs" },
								{ label: "Algorithms", slug: "docs/graphs/algorithms" },
								{ label: "Traversals", slug: "docs/graphs/traversals" },
								{ label: "Pattern matching", slug: "docs/graphs/pattern-matching" },
							],
						},
						{
							label: "CLI & Shell",
							collapsed: true,
							items: [
								{ label: "The shell", slug: "docs/cli/shell" },
								{ label: "Command line", slug: "docs/cli/command-line" },
							],
						},
						{
							label: "Database & Storage",
							collapsed: true,
							items: [
								{ label: "The database file", slug: "docs/database/database-file" },
								{ label: "Settings", slug: "docs/database/settings" },
							],
						},
						{
							label: "Coming from DuckDB",
							collapsed: true,
							items: [{ label: "Differences from DuckDB", slug: "docs/duckdb" }],
						},
						{
							label: "Reference",
							collapsed: true,
							items: [
								{ label: "Function reference", slug: "docs/reference/functions" },
								{ label: "Changelog", slug: "docs/changelog" },
								{ label: "Python library", link: "https://cldixon.github.io/ursa/" },
							],
						},
					],
				},
			],
		}),
	],
});
