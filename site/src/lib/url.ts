/**
 * Internal URLs. The site is served from the root of its Worker, so `url()` is
 * currently a pass-through — it stays as the single seam every component link
 * goes through, so a future prefix (or host move) is one change here rather
 * than a sweep of the components.
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
