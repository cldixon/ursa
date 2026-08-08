export const SITE = {
  name: 'Ursa',
  tagline: 'Polars-shaped dataframes for graph data',
  description:
    'Ursa is an in-memory, single-machine graph analytics library with a dataframe-first API. PageRank, centralities, components and traversals over edges in Parquet or CSV — Rust and Apache Arrow underneath.',
  repo: 'https://github.com/cldixon/ursa',
  pypi: 'https://pypi.org/project/ursa-graph/',
  pkg: 'ursa-graph',
} as const;

export interface NavItem {
  title: string;
  href: string;
  /** One mono line under the entry in the docs index. */
  blurb?: string;
}

export interface NavSection {
  title: string;
  items: NavItem[];
}

/**
 * The docs nav. "Catalog" is the reference section — numbered records, one per
 * object, which is what an API reference is.
 */
export const DOCS_NAV: NavSection[] = [
  {
    title: 'Start',
    items: [
      { title: 'Introduction', href: '/docs', blurb: 'What Ursa is, and what it is not.' },
      { title: 'Install', href: '/docs/install', blurb: 'Wheels bundle the Rust core.' },
      { title: 'Quickstart', href: '/docs/quickstart', blurb: 'A tour of the working surface.' },
      {
        title: 'Core concepts',
        href: '/docs/concepts',
        blurb: 'No Graph object; the algebra is closed over frames.',
      },
    ],
  },
  {
    title: 'Guides',
    items: [
      { title: 'Reading data', href: '/docs/guides/scans', blurb: 'Parquet, CSV, object storage.' },
      {
        title: 'Attributes & neighbours',
        href: '/docs/guides/attributes',
        blurb: 'Enrichment and neighbours().agg().',
      },
      {
        title: 'Traversals',
        href: '/docs/guides/traversals',
        blurb: 'hop, shortest_path, random_walk.',
      },
      { title: 'Weights', href: '/docs/guides/weights', blurb: 'Weight is an expression.' },
      {
        title: 'Whole-graph statistics',
        href: '/docs/guides/statistics',
        blurb: 'describe, density, diameter.',
      },
      { title: 'Getting results out', href: '/docs/guides/egress', blurb: 'Arrow all the way out.' },
    ],
  },
  {
    title: 'Catalog',
    items: [
      { title: 'API reference', href: '/docs/reference', blurb: 'Every public symbol, measured.' },
      {
        title: 'Semantics vs NetworkX',
        href: '/docs/semantics',
        blurb: 'The definitional choices, stated.',
      },
    ],
  },
  {
    title: 'Project',
    items: [
      { title: 'Architecture', href: '/docs/architecture', blurb: 'Arrow at the boundaries.' },
      { title: 'Benchmarks', href: '/docs/benchmarks', blurb: 'The flywheel, and its honesty.' },
      { title: 'Roadmap', href: '/docs/roadmap', blurb: 'What is built, and what is next.' },
    ],
  },
];

export const FLAT_NAV: NavItem[] = DOCS_NAV.flatMap((s) => s.items);
