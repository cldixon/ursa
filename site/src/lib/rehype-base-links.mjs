import { visit } from 'unist-util-visit';

/**
 * Prefix root-relative links in markdown with the configured `base`.
 *
 * Content pages link as `/docs/concepts`, which is how they read in the
 * repository and in any editor preview. The site is published under a
 * repository path, so those links need the prefix at build time — doing it here
 * keeps the prefix out of the prose entirely.
 */
export function rehypeBaseLinks(base) {
  const prefix = base.replace(/\/+$/, '');
  return () => (tree) => {
    if (!prefix) return;
    visit(tree, 'element', (node) => {
      if (node.tagName !== 'a') return;
      const href = node.properties?.href;
      if (typeof href !== 'string') return;
      if (!href.startsWith('/') || href.startsWith('//')) return;
      if (href.startsWith(`${prefix}/`) || href === prefix) return;
      node.properties.href = `${prefix}${href}`;
    });
  };
}
