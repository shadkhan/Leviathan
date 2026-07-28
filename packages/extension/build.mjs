/**
 * Extension build.
 *
 * esbuild, ~100 lines, no framework plugin chain. The output has to be
 * inspectable by a Chrome Web Store reviewer and by anyone reading the repo, so
 * the build stays small enough to read in one sitting.
 *
 *   node build.mjs            production bundle
 *   node build.mjs --watch    rebuild on change
 *   node build.mjs --no-check skip the bundle-size budget (for experiments)
 *
 * The `.wasm` is *copied*, never fetched at runtime: MV3 forbids remote code,
 * and a JSON viewer that makes network requests cannot honestly claim that
 * nothing leaves your machine.
 */

import { build, context } from 'esbuild';
import { cp, mkdir, readdir, readFile, rm } from 'node:fs/promises';
import { gzipSync } from 'node:zlib';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const root = dirname(fileURLToPath(import.meta.url));
const dist = resolve(root, 'dist');
const wasmDir = resolve(root, 'src/wasm');

const watch = process.argv.includes('--watch');
const check = !process.argv.includes('--no-check');

/**
 * Bundle-size budget, gzipped, excluding the `.wasm` (SPEC §M2, ADR-003).
 *
 * Enforced from M0 rather than measured at M7, because a budget introduced
 * after the fact is a budget that gets renegotiated instead of met.
 */
const BUDGET_GZIP_BYTES = 150 * 1024;

/** One bundle per execution context; they share no scope, so they share no file. */
const entryPoints = {
  viewer: 'src/ui/main.ts',
  worker: 'src/worker/index.ts',
  background: 'src/background/index.ts',
};

/** @type {import('esbuild').BuildOptions} */
const options = {
  entryPoints,
  outdir: dist,
  bundle: true,
  format: 'esm',
  // Chrome 111 is the manifest's floor: it is the first version with stable
  // `File System Access` write streams, which M6 needs for streaming export.
  target: 'chrome111',
  platform: 'browser',
  minify: !watch,
  sourcemap: watch ? 'inline' : false,
  legalComments: 'none',
  logLevel: 'info',
  metafile: true,
};

async function copyAssets() {
  await cp(resolve(root, 'public'), dist, { recursive: true });

  const wasm = join(wasmDir, 'leviathan_wasm_bg.wasm');
  await cp(wasm, join(dist, 'leviathan_wasm_bg.wasm')).catch(() => {
    throw new Error(
      'src/wasm/leviathan_wasm_bg.wasm is missing. Run `pnpm build:wasm` first — ' +
        'the WASM package is generated, not committed.',
    );
  });
}

/** Report gzipped sizes and fail the build if the JS/CSS budget is blown. */
async function report() {
  const files = await readdir(dist);
  let jsAndCss = 0;

  const rows = [];
  for (const name of files.sort()) {
    if (!/\.(js|css|wasm|html|json)$/.test(name)) continue;
    const bytes = await readFile(join(dist, name));
    const gz = gzipSync(bytes).byteLength;
    if (/\.(js|css)$/.test(name)) jsAndCss += gz;
    rows.push({ file: name, raw: bytes.byteLength, gzip: gz });
  }

  console.log('');
  for (const { file, raw, gzip } of rows) {
    console.log(
      `  ${file.padEnd(28)} ${String(raw).padStart(9)} B  ${String(gzip).padStart(8)} B gz`,
    );
  }

  const pct = Math.round((jsAndCss / BUDGET_GZIP_BYTES) * 100);
  console.log(
    `\n  JS+CSS budget: ${jsAndCss} / ${BUDGET_GZIP_BYTES} B gz (${pct}%)\n`,
  );

  if (check && jsAndCss > BUDGET_GZIP_BYTES) {
    throw new Error(
      `Bundle budget exceeded: ${jsAndCss} B gz > ${BUDGET_GZIP_BYTES} B gz. ` +
        'Cut something, or change the budget in build.mjs and say why in an ADR.',
    );
  }
}

await rm(dist, { recursive: true, force: true });
await mkdir(dist, { recursive: true });

if (watch) {
  await copyAssets();
  const ctx = await context(options);
  await ctx.watch();
  console.log('\n  watching — reload the unpacked extension after each rebuild\n');
} else {
  await build(options);
  await copyAssets();
  await report();
}
