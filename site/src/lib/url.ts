/**
 * Base-aware URLs. The site is published under a repository path on GitHub
 * Pages, so nothing links to a bare `/…` path directly.
 */

const BASE = import.meta.env.BASE_URL.replace(/\/+$/, '');

export function url(path: string): string {
  if (/^[a-z]+:/i.test(path) || path.startsWith('#')) return path;
  const p = path.startsWith('/') ? path : `/${path}`;
  return `${BASE}${p}` || '/';
}

/** True when `href` is the current page (or an ancestor section of it). */
export function isCurrent(href: string, pathname: string): boolean {
  const a = url(href).replace(/\/+$/, '');
  const b = pathname.replace(/\/+$/, '');
  return a === b;
}
