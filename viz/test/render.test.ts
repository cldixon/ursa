/**
 * Does it actually draw?
 *
 * The unit tests cover the arithmetic and the mapping rules, and none of them
 * would notice a shader that fails to compile, a buffer that never uploads, or a
 * camera framing empty space. Every one of those produces a clean, plausible,
 * entirely blank canvas — so this runs the real thing in headless Chromium and
 * reads the pixels back.
 *
 * It also exercises the bundling path the wheel will use: the harness is built
 * with `bun build --target=browser`, which is how the renderer will be shipped to
 * a notebook. A module that resolves under `bun test` but not under the bundler
 * fails here rather than at packaging time.
 *
 * Skipped, loudly, when Playwright or Chromium is absent, so the suite stays
 * runnable on a machine without a browser.
 */

import { afterAll, beforeAll, describe, expect, test } from 'bun:test';
import { existsSync } from 'node:fs';
import { mkdtemp, copyFile, rm } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';

const here = new URL('.', import.meta.url).pathname;

type Browser = { newPage(): Promise<Page>; close(): Promise<void> };
type Page = {
  goto(url: string): Promise<unknown>;
  waitForFunction(fn: string, arg?: unknown, opts?: unknown): Promise<unknown>;
  evaluate<T>(fn: string): Promise<T>;
  screenshot(opts: { path: string }): Promise<unknown>;
  mouse: {
    move(x: number, y: number): Promise<void>;
    down(): Promise<void>;
    up(): Promise<void>;
    wheel(dx: number, dy: number): Promise<void>;
  };
};

async function loadPlaywright(): Promise<{ chromium: { launch(o: unknown): Promise<Browser> } } | null> {
  try {
    return (await import('playwright')) as never;
  } catch {
    return null;
  }
}

const pw = await loadPlaywright();

/**
 * The browser to drive.
 *
 * Playwright pins a browser *build number* and refuses to launch against a
 * different one, so on a machine with a pre-installed Chromium — a CI image, this
 * container — the version it wants and the version present rarely match. Naming
 * the executable outright sidesteps the pin, and costs nothing when Playwright
 * did install its own: `PLAYWRIGHT_BROWSERS_PATH/chromium` is the stable symlink
 * either way.
 */
const chromiumPath =
  process.env.URSA_CHROMIUM ??
  join(process.env.PLAYWRIGHT_BROWSERS_PATH ?? '/opt/pw-browsers', 'chromium');
const canRun = pw !== null && existsSync(chromiumPath);

// `describe.skipIf` keeps the reason visible in the run rather than silently
// reporting a suite of zero tests.
describe.skipIf(!canRun)('renders in a real browser', () => {
  let browser: Browser;
  let page: Page;
  let dir: string;

  beforeAll(async () => {
    dir = await mkdtemp(join(tmpdir(), 'ursa-viz-'));
    const build = Bun.spawnSync([
      'bun',
      'build',
      join(here, 'harness.ts'),
      '--target=browser',
      '--format=iife',
      `--outfile=${join(dir, 'harness.js')}`,
    ]);
    if (build.exitCode !== 0) {
      throw new Error(`harness bundle failed:\n${build.stderr.toString()}`);
    }
    await copyFile(join(here, 'harness.html'), join(dir, 'harness.html'));
    await copyFile(join(here, '..', 'src', 'tokens.css'), join(dir, 'tokens.css'));

    browser = await pw!.chromium.launch({
      executablePath: chromiumPath,
      // SwiftShader: a CI runner has no GPU, so WebGL has to be software-rendered
      // or the context never comes back and every assertion here reads as "the
      // renderer is broken".
      args: ['--use-gl=angle', '--use-angle=swiftshader', '--enable-unsafe-swiftshader'],
    });
    page = await browser.newPage();
    await page.goto(`file://${join(dir, 'harness.html')}`);
    await page.waitForFunction('window.__ursaReady === true', undefined, { timeout: 30_000 });

    const err = await page.evaluate<string | null>('window.__ursaError ?? null');
    if (err != null) throw new Error(`harness threw:\n${err}`);
  }, 120_000);

  afterAll(async () => {
    await browser?.close();
    if (dir) await rm(dir, { recursive: true, force: true });
  });

  test('the WebGL context came up', async () => {
    const ok = await page.evaluate<boolean>(
      `!!document.querySelector('#mount-plate canvas')?.getContext('webgl')`,
    );
    expect(ok).toBe(true);
  });

  test('the graph reached the instrument', async () => {
    expect(await page.evaluate<number>('window.__ursa.nodeCount')).toBe(320);
    expect(await page.evaluate<number>('window.__ursa.edgeCount')).toBeGreaterThan(600);
  });

  test('pixels actually landed on both grounds', async () => {
    // The assertion this whole file exists for. A blank canvas passes every other
    // test in the package.
    const plate = await page.evaluate<number>(`window.__ursa.inkCoverage('plate')`);
    const sky = await page.evaluate<number>(`window.__ursa.inkCoverage('sky')`);
    expect(plate).toBeGreaterThan(0.01);
    expect(sky).toBeGreaterThan(0.01);
    // And not *everything* — full coverage would mean the clear colour is wrong
    // or a quad is covering the viewport, which also "draws".
    expect(plate).toBeLessThan(0.9);
    expect(sky).toBeLessThan(0.9);
  });

  test('the two grounds differ', async () => {
    // Same graph, same geometry: if the sky panel did not pick up
    // `[data-ground="sky"]`, the two canvases would be identical.
    const same = await page.evaluate<boolean>(`(() => {
      const read = (id) => {
        const c = document.querySelector('#' + id + ' canvas');
        const gl = c.getContext('webgl');
        const px = new Uint8Array(4);
        gl.readPixels(2, 2, 1, 1, gl.RGBA, gl.UNSIGNED_BYTE, px);
        return [px[0], px[1], px[2]].join(',');
      };
      window.__ursa.plate.drawNow();
      window.__ursa.sky.drawNow();
      return read('mount-plate') === read('mount-sky');
    })()`);
    expect(same).toBe(false);
  });

  test('the wheel zooms about the pointer', async () => {
    const before = await page.evaluate<number>('window.__ursa.plate.camera.scale');
    await page.mouse.move(300, 300);
    await page.mouse.wheel(0, -240);
    await page.evaluate<void>('new Promise((r) => requestAnimationFrame(() => r()))');
    const after = await page.evaluate<number>('window.__ursa.plate.camera.scale');
    expect(after).toBeGreaterThan(before);
  });

  test('dragging pans the view', async () => {
    const before = await page.evaluate<number>('window.__ursa.plate.camera.cx');
    await page.mouse.move(300, 300);
    await page.mouse.down();
    await page.mouse.move(380, 300);
    await page.mouse.up();
    await page.evaluate<void>('new Promise((r) => requestAnimationFrame(() => r()))');
    const after = await page.evaluate<number>('window.__ursa.plate.camera.cx');
    // Dragging right moves the camera left over the graph — content follows the
    // cursor.
    expect(after).toBeLessThan(before);
  });

  test('capturing a still', async () => {
    // Not an assertion so much as an artifact: on a failure the PNG is the
    // fastest way to see what the renderer actually produced.
    const out = process.env.URSA_VIZ_SNAPSHOT;
    if (!out) return expect(true).toBe(true);
    // Re-frame first: the pan and zoom tests above left the plate somewhere
    // deliberate, and a reference still should show the default framing.
    await page.evaluate<void>(
      'window.__ursa.plate.fit(); window.__ursa.sky.fit(); window.__ursa.plate.drawNow(); window.__ursa.sky.drawNow();',
    );
    await page.screenshot({ path: out });
    expect(true).toBe(true);
  });
});
