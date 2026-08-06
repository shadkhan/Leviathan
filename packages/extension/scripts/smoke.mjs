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

import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import { dirname, resolve } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const pkg = resolve(here, "../src/wasm");

// `pathToFileURL`, not a bare path: a dynamic import of `D:\...` is an
// unsupported URL scheme on Windows, and this repo is developed there.
const {
  default: init,
  Document,
  coreVersion,
  echo,
  rowLayoutVersion,
  sniffFormat,
} = await import(pathToFileURL(resolve(pkg, "leviathan_wasm.js")).href);

// In a browser this is a URL and the glue streams it. In Node there is no
// fetch of a file:// URL, so the bytes are passed directly — the same entry
// point, which is the reason the package is usable in both.
await init({
  module_or_path: await readFile(resolve(pkg, "leviathan_wasm_bg.wasm")),
});

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

check("coreVersion is semver", () => {
  assert.match(coreVersion(), /^\d+\.\d+\.\d+/);
});

check("echo round-trips the u32 range", () => {
  for (const value of [0, 1, 42, 2 ** 31, 4294967295]) {
    assert.equal(echo(value), value);
  }
});

const utf8 = new TextEncoder();

check("sniffFormat only returns values the TS union declares", () => {
  // Kept in step by hand with `Format` in src/protocol and
  // `leviathan_core::Format::as_str`. Three places, four values, one test.
  const declared = new Set(["single-document", "ndjson", "empty", "unknown"]);
  for (const input of [
    '{"a":1}',
    '{"a":1}\n{"a":2}\n',
    "   ",
    "<html>",
    "2026-07-27 INFO up",
  ]) {
    assert.ok(
      declared.has(sniffFormat(utf8.encode(input))),
      `undeclared for ${input}`,
    );
  }
});

check("sniffFormat agrees with the core test suite", () => {
  const cases = [
    ['{"a":1}', "single-document"],
    ["[1,2,3]", "single-document"],
    ['{"a":1}\n{"a":2}\n', "ndjson"],
    ["", "empty"],
    ["<html>", "unknown"],
    ["2026-07-27 INFO started", "unknown"],
  ];
  for (const [input, expected] of cases) {
    assert.equal(
      sniffFormat(utf8.encode(input)),
      expected,
      `for ${JSON.stringify(input)}`,
    );
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
  const view = new DataView(
    packed.buffer,
    packed.byteOffset,
    packed.byteLength,
  );
  const u64 = (at) =>
    view.getUint32(at, true) + view.getUint32(at + 4, true) * 2 ** 32;

  assert.equal(
    view.getUint32(0, true),
    rowLayoutVersion(),
    "layout version in the header",
  );
  const count = view.getUint32(4, true);
  const strings = view.getUint32(8, true);
  assert.equal(
    packed.byteLength,
    16 + count * 40 + strings,
    "buffer length matches its header",
  );

  const kinds = [
    "object",
    "array",
    "string",
    "number",
    "true",
    "false",
    "null",
    "invalid",
  ];
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
    const preview = decoder.decode(
      packed.subarray(cursor, cursor + previewLength),
    );
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

  assert.equal(
    cursor,
    packed.byteLength,
    "every string byte is claimed by a row",
  );
  return rows;
}

check("an NDJSON document indexes to one row per record", () => {
  const { document } = open('{"a":1}\n{"a":2}\n{"a":3}\n');
  assert.equal(document.format, "ndjson");
  assert.equal(document.rowCount(null), 3);
  document.free();
});

check("rows carry keys, previews and offsets back across the boundary", () => {
  const { document } = open(
    '{"name":"leviathan","size":500,"ok":true,"tags":[1,2,3]}',
  );
  const rows = unpack(document.rows(null, 0, 10));

  assert.equal(rows.length, 4);
  assert.deepEqual(
    rows.map((row) => row.key),
    ["name", "size", "ok", "tags"],
  );
  assert.equal(rows[0].kind, "string");
  assert.equal(rows[0].preview, "leviathan");
  assert.equal(rows[1].preview, "500");
  assert.equal(rows[2].kind, "true");
  assert.equal(rows[3].kind, "array");
  assert.equal(rows[3].children, 3);
  assert.ok(rows[3].childrenExact);
  assert.ok(rows[3].expandable);
  document.free();
});

check("a container expands to its children, addressed by byte offset", () => {
  const { document } = open('[[10,20,30],{"deep":true}]');
  const root = unpack(document.rows(null, 0, 10));
  assert.equal(root.length, 2);

  const step = document.expandStep(root[0].offset);
  assert.ok(
    step.done && step.complete,
    "a small container expands in one step",
  );
  assert.equal(step.children, 3);
  step.free();

  const children = unpack(document.rows(root[0].offset, 0, 10));
  assert.deepEqual(
    children.map((row) => row.preview),
    ["10", "20", "30"],
  );
  document.free();
});

check("a partial index is already browsable", () => {
  // The property the whole design is for: rows before the file is finished.
  // Deliberately larger than one batch (4 MB) — a smaller input finishes in a
  // single step and would assert nothing at all.
  const bytes = utf8.encode('{"n":1}\n'.repeat(700_000));
  const document = new Document(bytes.length, readerOver(bytes));

  const first = document.indexStep();
  const rowsSoFar = first.rows;
  const finished = first.done;
  first.free();

  assert.ok(rowsSoFar > 0, "the first batch found rows");
  assert.ok(!finished, "and the file is not finished");
  assert.equal(
    unpack(document.rows(null, 0, 5)).length,
    5,
    "which are readable now",
  );
  document.free();
});

check("a truncated document keeps the rows it found", () => {
  // C6: the file that is already damaged is the one the user most needs open.
  const { document } = open("[1,2,3,4");
  assert.equal(document.rowCount(null), 4);
  assert.deepEqual(
    unpack(document.rows(null, 0, 10)).map((row) => row.preview),
    ["1", "2", "3", "4"],
  );
  document.free();
});

check("the engine pulls ranges rather than being handed the file", () => {
  // If the engine ever stops asking, the memory model has gone with it.
  const { document, reader } = open('{"a":1}\n'.repeat(500));
  assert.ok(reader.reads > 0, "the engine asked the host for bytes");
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
    assert.ok(steps < 10_000, "findStep must terminate");
    if (last.done) {
      return { rows, steps, ...last };
    }
  }
}

check("a search finds matches and resolves them to rows", () => {
  const { document } = open(
    '{"id":1,"status":"ok"}\n{"id":2,"status":"error"}\n{"id":3,"status":"ok"}\n',
  );
  const found = findAll(document, "error");

  assert.equal(found.matches, 1, "one match");
  assert.deepEqual([...found.rows], [1], "in record 1, not 0 or 2");
  assert.equal(found.pending, 0, "nothing beyond the indexed region");
  assert.ok(!found.limited, "the cap was not reached");
  document.free();
});

check("a search reads the file, not the row previews", () => {
  // The property C47 exists for. A preview is truncated (C33), so a needle
  // buried deep in a long record is exactly what a preview-based search would
  // miss — and would miss silently, reporting "no matches" for a string that
  // is in the file.
  const buried = `{"pad":"${"x".repeat(4000)}","needle":"buried-treasure"}\n`;
  const { document } = open(`{"a":1}\n${buried}{"b":2}\n`);

  const found = findAll(document, "buried-treasure");
  assert.equal(found.matches, 1, "found 4 kB into a record");
  assert.deepEqual([...found.rows], [1]);
  document.free();
});

check("two hits in one record are two results but one row", () => {
  const { document } = open('{"a":"xx"}\n{"b":1}\n');
  const found = findAll(document, "x");
  assert.equal(found.matches, 2);
  assert.deepEqual([...found.rows], [0, 0], "the same row, reported twice");
  document.free();
});

check("search is case-insensitive on request and exact otherwise", () => {
  const { document } = open(
    '{"v":"Leviathan"}\n{"v":"LEVIATHAN"}\n{"v":"leviathan"}\n',
  );

  assert.equal(findAll(document, "leviathan", false).matches, 3, "folded");
  assert.equal(findAll(document, "leviathan", true).matches, 1, "exact");
  document.free();
});

check("a needle that is absent scans the whole file and finds nothing", () => {
  const text = '{"a":1}\n'.repeat(2000);
  const { document } = open(text);
  const found = findAll(document, "not-in-this-file");

  assert.equal(found.matches, 0);
  assert.equal(found.rows.length, 0);
  assert.equal(found.scanned, utf8.encode(text).length, "every byte was read");
  document.free();
});

check("starting a new search discards the one before it", () => {
  // What every keystroke in the find box does.
  const { document } = open('{"v":"alpha"}\n{"v":"beta"}\n');
  document.findStart("alpha", false, undefined);
  const superseded = findAll(document, "beta");

  assert.equal(superseded.matches, 1, "only the new needle is counted");
  assert.deepEqual([...superseded.rows], [1]);
  document.free();
});

/** Drive a filter to completion, collecting the rows each step reports. */
function filterAll(document, expression) {
  document.filterSet(expression);
  document.filterStart();
  const rows = [];
  let steps = 0;
  let last;
  for (;;) {
    const step = document.filterStep();
    rows.push(...step.rows);
    last = { matches: step.matches, done: step.done, limited: step.limited };
    step.free();
    steps++;
    assert.ok(steps < 10_000, "filterStep must terminate");
    if (last.done) {
      return { rows, steps, ...last };
    }
  }
}

const LOG = [
  '{"id":1,"level":"info","latency_ms":12,"meta":{"region":"eu-west-1"}}',
  '{"id":2,"level":"error","latency_ms":2400,"meta":{"region":"ap-south-1"}}',
  '{"id":3,"level":"error","latency_ms":80,"meta":{"region":"eu-west-1"}}',
  '{"id":4,"level":"warn","latency_ms":5000,"meta":{"region":"ap-south-1"}}',
  "",
].join("\n");

check("a filter selects records by condition, not by text", () => {
  const { document } = open(LOG);

  // The distinction the whole feature rests on: searching for `error` would also
  // return record 4 if the word appeared anywhere in it, and would miss nothing
  // only by luck. This asks a question about a field.
  const errors = filterAll(document, '@.level == "error"');
  assert.deepEqual([...errors.rows], [1, 2], "the two error records, by index");
  assert.equal(errors.matches, 2);
  assert.equal(errors.limited, false);

  document.free();
});

check("a filter compares numbers numerically", () => {
  const { document } = open(LOG);
  // `2400 > 999` is true; `"2400" > "999"` as text is false. A filter that got
  // this wrong would look like it worked on most data.
  assert.deepEqual([...filterAll(document, "@.latency_ms > 999").rows], [1, 3]);
  assert.deepEqual([...filterAll(document, "@.latency_ms <= 80").rows], [0, 2]);
  document.free();
});

check("conditions combine, and nested paths resolve", () => {
  const { document } = open(LOG);

  assert.deepEqual(
    [...filterAll(document, '@.level == "error" && @.latency_ms > 999').rows],
    [1],
    "both conditions, one record",
  );
  assert.deepEqual(
    [...filterAll(document, '@.meta.region == "ap-south-1" || @.id == 1').rows],
    [0, 1, 3],
  );
  assert.deepEqual(
    [...filterAll(document, '!(@.level == "info")').rows],
    [1, 2, 3],
  );
  assert.deepEqual(
    [...filterAll(document, "@.missing").rows],
    [],
    "existence of nothing",
  );

  document.free();
});

check("a filter that does not parse is refused, not guessed at", () => {
  const { document } = open(LOG);

  assert.throws(() => document.filterSet("@.level =="), /value/i);
  assert.throws(() => document.filterSet('@..level == "error"'), /descendant/i);
  assert.throws(() => document.filterSet("length(@.tags) > 1"), /function/i);

  // And the refusal leaves nothing half-armed behind it.
  assert.throws(() => document.filterStart(), /no filter/i);
  document.free();
});

check("a filter yields between batches and survives a malformed record", () => {
  // 5,000 records is three `filterStep` batches, which is what the Worker's
  // yielding depends on; the broken one in the middle must not stop the pass.
  const lines = [];
  for (let i = 0; i < 5_000; i++) {
    lines.push(i === 2_500 ? '{"n":' : `{"n":${i}}`);
  }
  const { document } = open(`${lines.join("\n")}\n`);

  const run = filterAll(document, "@.n >= 4990");
  assert.ok(run.steps >= 3, `expected several steps, got ${run.steps}`);
  assert.equal(run.matches, 10, "the last ten records match");
  assert.ok(
    !run.rows.includes(2_500),
    "the unparseable record simply does not match",
  );

  document.free();
});

/** Drive a duplicate pass to completion, collecting what it reports. */
function dedupAll(document, { keys = true, elements = false } = {}) {
  document.dedupStart(keys, elements);
  const found = [];
  let steps = 0;
  let last;
  for (;;) {
    const step = document.dedupStep();
    const messages = step.messages === "" ? [] : step.messages.split("");
    messages.forEach((entry, at) => {
      const [kind, what] = entry.split("");
      found.push({
        kind,
        what,
        first: step.positions[at * 4],
        firstRow: step.positions[at * 4 + 1],
        second: step.positions[at * 4 + 2],
        secondRow: step.positions[at * 4 + 3],
      });
    });
    last = { total: step.found, done: step.done, capped: step.capped };
    step.free();
    steps++;
    assert.ok(steps < 10_000, "dedupStep must terminate");
    if (last.done) {
      return { found, steps, ...last };
    }
  }
}

check("a repeated key is reported with both of its locations", () => {
  // Valid JSON that every parser resolves differently, and the only thing in
  // this product that says so.
  const { document } = open('{"id":1,"name":"a","id":2}');
  const run = dedupAll(document);

  assert.equal(run.total, 1);
  assert.equal(run.found.length, 1);
  assert.equal(run.found[0].kind, "key");
  assert.equal(run.found[0].what, "id");
  assert.equal(run.found[0].first, 1, "the first `id`");
  assert.equal(run.found[0].second, 19, "and the repeat");
  document.free();
});

check("the same key in different records is not a duplicate", () => {
  // Every record in a log has an `id`. Reporting that would make the feature
  // useless on exactly the files it exists for.
  const { document } = open('{"id":1}\n{"id":2}\n{"id":3}\n');
  assert.equal(dedupAll(document).total, 0);
  document.free();
});

check("duplicate offsets resolve to rows the tree can be sent to", () => {
  const { document } = open('{"a":1}\n{"b":2,"b":3}\n{"c":4}\n');
  const run = dedupAll(document);

  assert.equal(run.total, 1);
  assert.equal(run.found[0].what, "b");
  assert.equal(run.found[0].secondRow, 1, "the middle record");
  assert.equal(run.found[0].firstRow, 1, "both occurrences are in it");
  document.free();
});

check("element checking is opt-in, and finds repeated records", () => {
  const text = '{"a":1}\n{"b":2}\n{"a":1}\n';

  const { document: keysOnly } = open(text);
  assert.equal(
    dedupAll(keysOnly).total,
    0,
    "off by default — it is the slow half",
  );
  keysOnly.free();

  const { document } = open(text);
  const run = dedupAll(document, { elements: true });
  assert.equal(run.total, 1);
  assert.equal(run.found[0].kind, "element");
  assert.equal(run.found[0].second, 16, "the third record repeats the first");
  document.free();
});

check("a duplicate pass yields between batches on a large file", () => {
  // Larger than one 8 MiB batch, deliberately: a pass that finishes in a single
  // step proves nothing about yielding, and 20 000 records (500 KB) did exactly
  // that. Each record repeats a key, so the expected count is known exactly.
  const RECORDS = 400_000;
  const lines = [];
  for (let i = 0; i < RECORDS; i++) {
    lines.push(`{"id":${i},"dup":1,"dup":2}`);
  }
  const { document } = open(`${lines.join("\n")}\n`);

  const run = dedupAll(document);
  assert.equal(run.total, RECORDS, "one repeat per record, counted exactly");
  assert.ok(run.steps > 1, `expected several steps, got ${run.steps}`);
  assert.ok(
    run.found.length < run.total,
    "and the listing is capped while the count is not",
  );
  document.free();
});

/** Drive an export to completion, returning the concatenated bytes. */
function exportAll(document, format, rows = []) {
  document.exportStart(format, new Float64Array(rows));
  const parts = [];
  let steps = 0;
  let last;
  for (;;) {
    const step = document.exportStep();
    parts.push(step.chunk.slice());
    last = {
      records: step.records,
      done: step.done,
      truncated: step.truncated,
    };
    step.free();
    steps++;
    assert.ok(steps < 10_000, "exportStep must terminate");
    if (last.done) {
      const total = parts.reduce((n, part) => n + part.length, 0);
      const bytes = new Uint8Array(total);
      let at = 0;
      for (const part of parts) {
        bytes.set(part, at);
        at += part.length;
      }
      return { text: new TextDecoder().decode(bytes), steps, ...last };
    }
  }
}

check("an export re-parses to exactly what went in", () => {
  // Requirement 11, checked rather than asserted — and checked on the values a
  // float round-trip would quietly change.
  const source = [
    '{"n":1.0000000000000002,"big":10000000000000000000}',
    '{"s":"\\u0041","raw":"A","esc":"\\/"}',
    '{"deep":{"a":[1,2,{"b":null}]},"t":true}',
    "",
  ].join("\n");

  const { document } = open(source);
  const run = exportAll(document, "ndjson");

  assert.equal(run.records, 3);
  assert.equal(run.truncated, false);
  assert.equal(
    run.text,
    source,
    "byte-identical: the source was already minified",
  );
  document.free();
});

check("whitespace is the only thing minifying removes", () => {
  const { document } = open('{ "a" : 1 , "b" : [ 2 , 3 ] }\n');
  assert.equal(exportAll(document, "ndjson").text, '{"a":1,"b":[2,3]}\n');
  document.free();
});

check(
  "json wraps the records, pretty prints them, and still round-trips",
  () => {
    const { document } = open('{"a":1}\n{"b":[2]}\n');

    assert.equal(exportAll(document, "json").text, '[{"a":1},{"b":[2]}]');

    const pretty = exportAll(document, "json-pretty").text;
    assert.ok(pretty.includes('"a": 1'), pretty);
    assert.ok(pretty.includes("\n  {"), pretty);
    // Same document, differently spaced: re-minifying gets back to the other one.
    assert.equal(
      JSON.stringify(JSON.parse(pretty)),
      '[{"a":1},{"b":[2]}]',
      pretty,
    );
    document.free();
  },
);

check(
  "csv discovers columns across every record before writing a header",
  () => {
    // The column that first appears last still belongs in the header, which is
    // why discovery is its own pass and why `exportStep` drives both.
    const { document } = open(
      '{"a":1}\n{"a":2,"meta":{"r":"eu"}}\n{"tags":["x","y"]}\n',
    );
    const run = exportAll(document, "csv");
    const lines = run.text.split("\r\n");

    assert.equal(lines[0], "a,meta.r,tags");
    assert.equal(lines[1], "1,,");
    assert.equal(lines[2], "2,eu,");
    assert.equal(
      lines[3],
      ',,"[""x"",""y""]"',
      "an array is one cell, not many",
    );
    document.free();
  },
);

check("a single document exports as itself, not as its members", () => {
  // The bug this test exists for: a root *object*'s tier-1 rows are its
  // members, so exporting them as a sequence wrote `"a":1` per line and dropped
  // the braces entirely. Its rows are only records when the root is an array.
  const { document } = open('{ "a" : 1 , "b" : [ 2 , 3 ] }');
  assert.equal(exportAll(document, "ndjson").text, '{"a":1,"b":[2,3]}\n');
  assert.equal(
    exportAll(document, "json").text,
    '{"a":1,"b":[2,3]}',
    "and it is not wrapped in an array it never had",
  );
  document.free();
});

check("a root array exports one element per line as NDJSON", () => {
  // The conversion that is actually useful, and the reason the root's kind is
  // consulted rather than assumed.
  const { document } = open('[{"a":1},{"a":2},{"a":3}]');
  assert.equal(
    exportAll(document, "ndjson").text,
    '{"a":1}\n{"a":2}\n{"a":3}\n',
  );
  assert.equal(
    exportAll(document, "json").text,
    '[{"a":1},{"a":2},{"a":3}]',
    "and back to the array it came from",
  );
  document.free();
});

check("an export can be restricted to chosen rows", () => {
  // What "export the current filter result" is built on.
  const { document } = open('{"n":0}\n{"n":1}\n{"n":2}\n{"n":3}\n');
  const run = exportAll(document, "ndjson", [1, 3]);

  assert.equal(run.records, 2);
  assert.equal(run.text, '{"n":1}\n{"n":3}\n');
  document.free();
});

check("a large export arrives in batches rather than all at once", () => {
  // The property the streaming write depends on: if this came back in one
  // chunk, a 500 MB export would be assembled in memory before a byte reached
  // the disk.
  const lines = [];
  for (let i = 0; i < 5_000; i++) {
    lines.push(`{"id":${i}}`);
  }
  const { document } = open(`${lines.join("\n")}\n`);

  const run = exportAll(document, "ndjson");
  assert.equal(run.records, 5_000);
  assert.ok(run.steps > 2, `expected several steps, got ${run.steps}`);
  document.free();
});

check("an unknown export format is refused rather than guessed", () => {
  const { document } = open('{"a":1}\n');
  assert.throws(
    () => document.exportStart("yaml", new Float64Array(0)),
    /yaml/,
  );
  document.free();
});

check("the row layout version is the one the extension bundle expects", () => {
  // The bundle's copy lives in src/protocol/rows.ts. Two constants, one layout;
  // this is the seam where a stale `dist/` shows up as an error rather than as
  // wrong rows.
  assert.equal(rowLayoutVersion(), 1);
});

console.log(`\nleviathan-wasm smoke test (engine ${coreVersion()})\n`);
console.log(checks.join("\n"));
console.log(process.exitCode ? "\nFAILED\n" : "\nall good\n");
