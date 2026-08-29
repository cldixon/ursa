/**
 * The release this documentation describes.
 *
 * Derived from the workspace `Cargo.toml` — the same single source the Python
 * distribution takes `__version__` from, via `dynamic = ["version"]` and
 * maturin. Hand-written version strings in prose drift silently the moment a
 * release is cut, and the site is what a new user reads before installing
 * anything, so the number is derived rather than typed.
 *
 * The read happens in `astro.config.mjs`, which is not bundled, and arrives here
 * as a compile-time constant: nothing touches the filesystem at render time and
 * no path has to survive bundling into `dist/`.
 */

declare const __URSA_VERSION__: string;

/** e.g. `0.3.0` — the released version this build of the site documents. */
export const VERSION: string = __URSA_VERSION__;

/** The tag for {@link VERSION}, e.g. `v0.3.0`. */
export const VERSION_TAG = `v${VERSION}`;
