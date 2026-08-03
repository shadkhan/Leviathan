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
const { default: init, Document, coreVersion, echo, rowLayoutVersion, sniffFormat } = await import(
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

/*
 * The boundary carrying real work.
 *
 * Everything above is a scalar crossing a line. What follows is the actual
 * product shape: a source of bytes the engine pulls from, an index built in
 * batches, and rows decoded out of a packed buffer. In the extension the reader
 * is a `FileReaderSync` over a `File`; here it is a `Buffer`. The engine cannot
 * tell the difference, which is the sans-IO claim being exercised rather than
 * asserted (DEEP_REASONING.md C2, C42).
 */

/** The host side of the `ByteReader` contract: synchronous, clamped at the end. */
function readerOver(bytes) {
  return {
    reads: 0,
    read(start, length) {
      this.reads++;
      return bytes.subarray(start, Math.min(start + length, bytes.length));
    },
  };
}

/** Index a whole buffer and return the open document. */
function open(text) {
  const bytes = utf8.encode(text);
  const reader = readerOver(bytes);
  const document = new Document(bytes.length, reader);
  let steps = 0;
  for (;;) {
    const step = document.indexStep();
    const done = step.done;
    step.free();
    steps++;
    if (done) {
      break;
    }
  }
  return { document, reader, steps };
}

/**
 * Decode a packed row buffer.
 *
 * Deliberately a second implementation rather than an import of the extension's
 * `RowBlock`: two independent readers of one layout agreeing is evidence, where
 * one reader agreeing with itself is not. Kept in step with
 * `leviathan-wasm/src/pack.rs`, which is the specification.
 */
function unpack(packed) {
  const view = new DataView(packed.buffer, packed.byteOffset, packed.byteLength);
  const u64 = (at) => view.getUint32(at, true) + view.getUint32(at + 4, true) * 2 ** 32;

  assert.equal(view.getUint32(0, true), rowLayoutVersion(), 'layout version in the header');
  const count = view.getUint32(4, true);
  const strings = view.getUint32(8, true);
  assert.equal(packed.byteLength, 16 + count * 40 + strings, 'buffer length matches its header');

  const kinds = ['object', 'array', 'string', 'number', 'true', 'false', 'null', 'invalid'];
  const decoder = new TextDecoder();
  const rows = [];
  let cursor = 16 + count * 40;

  for (let i = 0; i < count; i++) {
    const at = 16 + i * 40;
    const flags = view.getUint8(at + 33);
    const keyLength = view.getUint16(at + 34, true);
    const previewLength = view.getUint32(at + 36, true);
    const key = decoder.decode(packed.subarray(cursor, cursor + keyLength));
    cursor += keyLength;
    const preview = decoder.decode(packed.subarray(cursor, cursor + previewLength));
    cursor += previewLength;

    rows.push({
      offset: u64(at),
      kind: kinds[view.getUint8(at + 32)],
      key: (flags & 4) === 0 ? null : key,
      preview,
      children: u64(at + 24),
      childrenExact: (flags & 1) !== 0,
      expandable: (flags & 8) !== 0,
    });
  }

  assert.equal(cursor, packed.byteLength, 'every string byte is claimed by a row');
  return rows;
}

check('an NDJSON document indexes to one row per record', () => {
  const { document } = open('{"a":1}\n{"a":2}\n{"a":3}\n');
  assert.equal(document.format, 'ndjson');
  assert.equal(document.rowCount(null), 3);
  document.free();
});

check('rows carry keys, previews and offsets back across the boundary', () => {
  const { document } = open('{"name":"leviathan","size":500,"ok":true,"tags":[1,2,3]}');
  const rows = unpack(document.rows(null, 0, 10));

  assert.equal(rows.length, 4);
  assert.deepEqual(
    rows.map((row) => row.key),
    ['name', 'size', 'ok', 'tags'],
  );
  assert.equal(rows[0].kind, 'string');
  assert.equal(rows[0].preview, 'leviathan');
  assert.equal(rows[1].preview, '500');
  assert.equal(rows[2].kind, 'true');
  assert.equal(rows[3].kind, 'array');
  assert.equal(rows[3].children, 3);
  assert.ok(rows[3].childrenExact);
  assert.ok(rows[3].expandable);
  document.free();
});

check('a container expands to its children, addressed by byte offset', () => {
  const { document } = open('[[10,20,30],{"deep":true}]');
  const root = unpack(document.rows(null, 0, 10));
  assert.equal(root.length, 2);

  const step = document.expandStep(root[0].offset);
  assert.ok(step.done && step.complete, 'a small container expands in one step');
  assert.equal(step.children, 3);
  step.free();

  const children = unpack(document.rows(root[0].offset, 0, 10));
  assert.deepEqual(
    children.map((row) => row.preview),
    ['10', '20', '30'],
  );
  document.free();
});

check('a partial index is already browsable', () => {
  // The property the whole design is for: rows before the file is finished.
  // Deliberately larger than one batch (4 MB) — a smaller input finishes in a
  // single step and would assert nothing at all.
  const bytes = utf8.encode('{"n":1}\n'.repeat(700_000));
  const document = new Document(bytes.length, readerOver(bytes));

  const first = document.indexStep();
  const rowsSoFar = first.rows;
  const finished = first.done;
  first.free();

  assert.ok(rowsSoFar > 0, 'the first batch found rows');
  assert.ok(!finished, 'and the file is not finished');
  assert.equal(unpack(document.rows(null, 0, 5)).length, 5, 'which are readable now');
  document.free();
});

check('a truncated document keeps the rows it found', () => {
  // C6: the file that is already damaged is the one the user most needs open.
  const { document } = open('[1,2,3,4');
  assert.equal(document.rowCount(null), 4);
  assert.deepEqual(
    unpack(document.rows(null, 0, 10)).map((row) => row.preview),
    ['1', '2', '3', '4'],
  );
  document.free();
});

check('the engine pulls ranges rather than being handed the file', () => {
  // If the engine ever stops asking, the memory model has gone with it.
  const { document, reader } = open('{"a":1}\n'.repeat(500));
  assert.ok(reader.reads > 0, 'the engine asked the host for bytes');
  document.free();
});

/** Drive a search to completion, collecting the rows each step reports. */
function findAll(document, needle, caseSensitive = false) {
  document.findStart(needle, caseSensitive, undefined);
  const rows = [];
  let steps = 0;
  let last;
  for (;;) {
    const step = document.findStep();
    rows.push(...step.rows);
    last = {
      matches: step.matches,
      pending: step.pending,
      scanned: step.scanned,
      done: step.done,
      limited: step.limited,
    };
    step.free();
    steps++;
    assert.ok(steps < 10_000, 'findStep must terminate');
    if (last.done) {
      return { rows, steps, ...last };
    }
  }
}

check('a search finds matches and resolves them to rows', () => {
  const { document } = open(
    '{"id":1,"status":"ok"}\n{"id":2,"status":"error"}\n{"id":3,"status":"ok"}\n',
  );
  const found = findAll(document, 'error');

  assert.equal(found.matches, 1, 'one match');
  assert.deepEqual([...found.rows], [1], 'in record 1, not 0 or 2');
  assert.equal(found.pending, 0, 'nothing beyond the indexed region');
  assert.ok(!found.limited, 'the cap was not reached');
  document.free();
});

check('a search reads the file, not the row previews', () => {
  // The property C47 exists for. A preview is truncated (C33), so a needle
  // buried deep in a long record is exactly what a preview-based search would
  // miss — and would miss silently, reporting "no matches" for a string that
  // is in the file.
  const buried = `{"pad":"${'x'.repeat(4000)}","needle":"buried-treasure"}\n`;
  const { document } = open(`{"a":1}\n${buried}{"b":2}\n`);

  const found = findAll(document, 'buried-treasure');
  assert.equal(found.matches, 1, 'found 4 kB into a record');
  assert.deepEqual([...found.rows], [1]);
  document.free();
});

check('two hits in one record are two results but one row', () => {
  const { document } = open('{"a":"xx"}\n{"b":1}\n');
  const found = findAll(document, 'x');
  assert.equal(found.matches, 2);
  assert.deepEqual([...found.rows], [0, 0], 'the same row, reported twice');
  document.free();
});

check('search is case-insensitive on request and exact otherwise', () => {
  const { document } = open('{"v":"Leviathan"}\n{"v":"LEVIATHAN"}\n{"v":"leviathan"}\n');

  assert.equal(findAll(document, 'leviathan', false).matches, 3, 'folded');
  assert.equal(findAll(document, 'leviathan', true).matches, 1, 'exact');
  document.free();
});

check('a needle that is absent scans the whole file and finds nothing', () => {
  const text = '{"a":1}\n'.repeat(2000);
  const { document } = open(text);
  const found = findAll(document, 'not-in-this-file');

  assert.equal(found.matches, 0);
  assert.equal(found.rows.length, 0);
  assert.equal(found.scanned, utf8.encode(text).length, 'every byte was read');
  document.free();
});

check('starting a new search discards the one before it', () => {
  // What every keystroke in the find box does.
  const { document } = open('{"v":"alpha"}\n{"v":"beta"}\n');
  document.findStart('alpha', false, undefined);
  const superseded = findAll(document, 'beta');

  assert.equal(superseded.matches, 1, 'only the new needle is counted');
  assert.deepEqual([...superseded.rows], [1]);
  document.free();
});

check('the row layout version is the one the extension bundle expects', () => {
  // The bundle's copy lives in src/protocol/rows.ts. Two constants, one layout;
  // this is the seam where a stale `dist/` shows up as an error rather than as
  // wrong rows.
  assert.equal(rowLayoutVersion(), 1);
});

console.log(`\nleviathan-wasm smoke test (engine ${coreVersion()})\n`);
console.log(checks.join('\n'));
console.log(process.exitCode ? '\nFAILED\n' : '\nall good\n');
