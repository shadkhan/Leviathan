/**
 * Headless smoke test for the built WASM package.
 *
 *   node scripts/smoke.mjs
 *
 * This does not replace loading the extension — CSP, the manifest, and the
 * Worker boundary can only be exercised in a browser. What it does is fail CI
 * on the failures that are worth catching in one second rather than one manual
 * click: a `.wasm` that does not instantiate, a binding that was renamed, or a
 * `Format` string that the TypeScript union does not cover.
 *
 * It doubles as the "no extension required" usage example SPEC §M7 owes the npm
 * package: this file is what consuming `@shadkhan/leviathan-core` from Node
 * looks like, start to finish.
 */

import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import { dirname, resolve } from 'node:path';
import { fileURLToPath, pathToFileURL } from 'node:url';

const here = dirname(fileURLToPath(import.meta.url));
const pkg = resolve(here, '../src/wasm');

// `pathToFileURL`, not a bare path: a dynamic import of `D:\...` is an
// unsupported URL scheme on Windows, and this repo is developed there.
const { default: init, coreVersion, echo, sniffFormat } = await import(
  pathToFileURL(resolve(pkg, 'leviathan_wasm.js')).href
);

// In a browser this is a URL and the glue streams it. In Node there is no
// fetch of a file:// URL, so the bytes are passed directly — the same entry
// point, which is the reason the package is usable in both.
await init({ module_or_path: await readFile(resolve(pkg, 'leviathan_wasm_bg.wasm')) });

const checks = [];
const check = (name, fn) => {
  try {
    fn();
    checks.push(`  ok    ${name}`);
  } catch (error) {
    checks.push(`  FAIL  ${name}\n        ${error.message}`);
    process.exitCode = 1;
  }
};

check('coreVersion is semver', () => {
  assert.match(coreVersion(), /^\d+\.\d+\.\d+/);
});

check('echo round-trips the u32 range', () => {
  for (const value of [0, 1, 42, 2 ** 31, 4294967295]) {
    assert.equal(echo(value), value);
  }
});

const utf8 = new TextEncoder();

check('sniffFormat only returns values the TS union declares', () => {
  // Kept in step by hand with `Format` in src/protocol and
  // `leviathan_core::Format::as_str`. Three places, four values, one test.
  const declared = new Set(['single-document', 'ndjson', 'empty', 'unknown']);
  for (const input of ['{"a":1}', '{"a":1}\n{"a":2}\n', '   ', '<html>', '2026-07-27 INFO up']) {
    assert.ok(declared.has(sniffFormat(utf8.encode(input))), `undeclared for ${input}`);
  }
});

check('sniffFormat agrees with the core test suite', () => {
  const cases = [
    ['{"a":1}', 'single-document'],
    ['[1,2,3]', 'single-document'],
    ['{"a":1}\n{"a":2}\n', 'ndjson'],
    ['', 'empty'],
    ['<html>', 'unknown'],
    ['2026-07-27 INFO started', 'unknown'],
  ];
  for (const [input, expected] of cases) {
    assert.equal(sniffFormat(utf8.encode(input)), expected, `for ${JSON.stringify(input)}`);
  }
});

console.log(`\nleviathan-wasm smoke test (engine ${coreVersion()})\n`);
console.log(checks.join('\n'));
console.log(process.exitCode ? '\nFAILED\n' : '\nall good\n');
