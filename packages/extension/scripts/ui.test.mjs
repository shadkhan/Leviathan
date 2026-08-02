/**
 * Tests for the two pieces of the viewer that are pure computation.
 *
 *   node scripts/ui.test.mjs
 *
 * `Tree` and `RowStore` decide what row 4 812 907 is and where its bytes come
 * from. Neither touches the DOM, and neither should have to be exercised by
 * clicking a browser to find out that it is wrong — the arithmetic in `Tree` is
 * the kind that is either right for every index or subtly wrong for a few, and
 * "subtly wrong for a few" is invisible until someone scrolls to the wrong
 * place in a 500 MB file.
 *
 * The modules are TypeScript, so esbuild — already the extension's bundler —
 * compiles them in memory and the result is imported as a data URL. No build
 * artefact, no test framework, no new dependency.
 */

import assert from 'node:assert/strict';
import { build } from 'esbuild';
import { dirname, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const root = resolve(dirname(fileURLToPath(import.meta.url)), '..');

const bundled = await build({
  stdin: {
    contents: `
      export { Tree } from './src/ui/tree.js';
      export { RowStore, BLOCK_ROWS } from './src/ui/store.js';
      export { Search, describeSearch } from './src/ui/search.js';
    `,
    resolveDir: root,
    loader: 'ts',
  },
  bundle: true,
  format: 'esm',
  platform: 'neutral',
  target: 'es2022',
  write: false,
  logLevel: 'warning',
});

const source = bundled.outputFiles[0].text;
const { Tree, RowStore, BLOCK_ROWS, Search, describeSearch } = await import(
  `data:text/javascript;base64,${Buffer.from(source).toString('base64')}`
);

const checks = [];
const check = async (name, fn) => {
  try {
    await fn();
    checks.push(`  ok    ${name}`);
  } catch (error) {
    checks.push(`  FAIL  ${name}\n        ${error.message}`);
    process.exitCode = 1;
  }
};

/* ------------------------------------------------------------------ Tree */

/** Open a container at `index` of `branch`, addressed by a made-up offset. */
function openAt(tree, branch, index, count, offset = (index + 1) * 1000) {
  return tree.open({ branch, index, depth: 0 }, offset, count, true);
}

await check('an empty tree has no rows', () => {
  const tree = new Tree();
  assert.equal(tree.size, 0);
});

await check('a flat root is its own row count', () => {
  const tree = new Tree();
  tree.setCount(tree.root, 5, true);
  assert.equal(tree.size, 5);
  for (let i = 0; i < 5; i++) {
    const at = tree.locate(i);
    assert.equal(at.index, i);
    assert.equal(at.depth, 0);
    assert.equal(at.branch.container, null);
  }
});

await check('opening a container inserts its children after it', () => {
  const tree = new Tree();
  tree.setCount(tree.root, 5, true);
  const branch = openAt(tree, tree.root, 2, 3);

  assert.equal(tree.size, 8, '5 root rows plus 3 children');

  // 0 1 2 [c0 c1 c2] 3 4
  assert.deepEqual(
    [0, 1, 2, 3, 4, 5, 6, 7].map((i) => {
      const at = tree.locate(i);
      return `${at.depth}:${at.index}`;
    }),
    ['0:0', '0:1', '0:2', '1:0', '1:1', '1:2', '0:3', '0:4'],
  );
  assert.equal(tree.locate(4).branch, branch);
});

await check('a container that grows makes the tree taller, not different', () => {
  const tree = new Tree();
  tree.setCount(tree.root, 3, true);
  const branch = openAt(tree, tree.root, 0, 2);
  assert.equal(tree.size, 5);

  tree.setCount(branch, 10, false);
  assert.equal(tree.size, 13);
  assert.equal(tree.locate(11).index, 1, 'the root rows after it moved down');
  assert.equal(tree.locate(11).depth, 0);
});

await check('nested containers nest', () => {
  const tree = new Tree();
  tree.setCount(tree.root, 2, true);
  const outer = openAt(tree, tree.root, 0, 2, 100);
  const inner = openAt(tree, outer, 1, 2, 200);

  // 0 [c0 c1 [g0 g1]] 1
  assert.equal(tree.size, 6);
  assert.equal(tree.locate(3).branch, inner);
  assert.equal(tree.locate(3).depth, 2);
  assert.equal(tree.locate(5).depth, 0);
  assert.equal(tree.locate(5).index, 1);
});

await check('closing a container takes its whole subtree with it', () => {
  const tree = new Tree();
  tree.setCount(tree.root, 2, true);
  const outer = openAt(tree, tree.root, 0, 2, 100);
  openAt(tree, outer, 1, 2, 200);

  const forgotten = tree.close(outer);
  assert.deepEqual(forgotten.sort((a, b) => a - b), [100, 200]);
  assert.equal(tree.size, 2, 'back to the root rows');
  assert.equal(tree.branchOf(200), undefined, 'and the descendant is not open');
});

await check('the root cannot be closed', () => {
  const tree = new Tree();
  tree.setCount(tree.root, 3, true);
  assert.deepEqual(tree.close(tree.root), []);
  assert.equal(tree.size, 3);
});

await check('flatIndexOf is the exact inverse of locate, everywhere', () => {
  // The property the virtual list and the keyboard both depend on: a row's
  // position and a row's identity have to be the same fact read two ways. A
  // deterministic pseudo-random tree, so a failure is reproducible.
  let seed = 0x2545f491;
  const random = (n) => {
    seed = (seed * 1103515245 + 12345) & 0x7fffffff;
    return seed % n;
  };

  const tree = new Tree();
  tree.setCount(tree.root, 40, true);
  const branches = [tree.root];
  for (let opened = 0; opened < 12; opened++) {
    const branch = branches[random(branches.length)];
    if (branch.count === 0) continue;
    const index = random(branch.count);
    if (branch.children.some((child) => child.indexInParent === index)) continue;
    branches.push(openAt(tree, branch, index, 1 + random(9), 1000 + opened));
  }

  assert.ok(tree.size > 40, 'the tree actually opened something');
  for (let flat = 0; flat < tree.size; flat++) {
    const at = tree.locate(flat);
    assert.equal(tree.flatIndexOf(at.branch, at.index), flat, `round trip at ${flat}`);
  }
});

/* -------------------------------------------------------------- RowStore */

/**
 * The packed row layout, written rather than read.
 *
 * A third implementation of `pack.rs`'s layout, deliberately: the extension's
 * decoder and the engine's encoder agreeing with each other proves the layout
 * travels, and this proves the decoder is not simply mirroring a mistake.
 */
function pack(rows) {
  const encoder = new TextEncoder();
  const encoded = rows.map((row) => ({
    ...row,
    keyBytes: encoder.encode(row.key ?? ''),
    previewBytes: encoder.encode(row.preview ?? ''),
  }));
  const stringBytes = encoded.reduce(
    (total, row) => total + row.keyBytes.length + row.previewBytes.length,
    0,
  );

  const buffer = new ArrayBuffer(16 + rows.length * 40 + stringBytes);
  const view = new DataView(buffer);
  const bytes = new Uint8Array(buffer);

  view.setUint32(0, 1, true);
  view.setUint32(4, rows.length, true);
  view.setUint32(8, stringBytes, true);

  let cursor = 16 + rows.length * 40;
  encoded.forEach((row, i) => {
    const at = 16 + i * 40;
    view.setUint32(at, row.offset, true);
    view.setUint32(at + 8, row.offset, true);
    view.setUint32(at + 16, row.offset + 1, true);
    view.setUint32(at + 24, 0, true);
    view.setUint8(at + 32, 3); // number
    view.setUint8(at + 33, row.key === undefined ? 1 : 1 | 4);
    view.setUint16(at + 34, row.keyBytes.length, true);
    view.setUint32(at + 36, row.previewBytes.length, true);

    bytes.set(row.keyBytes, cursor);
    cursor += row.keyBytes.length;
    bytes.set(row.previewBytes, cursor);
    cursor += row.previewBytes.length;
  });

  return buffer;
}

/**
 * An engine that behaves the way the real one does in the ways that matter:
 * expansion advances in batches, and an evicted container reports nothing until
 * it is expanded again (C36).
 */
class FakeEngine {
  constructor(total, batch = 50) {
    this.total = total;
    this.batch = batch;
    this.resident = 0;
    this.calls = { rows: 0, expand: 0, forget: 0 };
  }

  /** What the engine's expansion cache dropping this container looks like. */
  evict() {
    this.resident = 0;
  }

  async call(method, params) {
    this.calls[method] = (this.calls[method] ?? 0) + 1;
    if (method === 'expand') {
      this.resident = Math.min(this.total, this.resident + this.batch);
      const done = this.resident >= this.total;
      return { children: this.resident, done, complete: done };
    }
    if (method === 'rows') {
      const end = Math.min(params.start + params.count, this.resident);
      const rows = [];
      for (let i = params.start; i < end; i++) {
        rows.push({ offset: i * 10, key: `k${i}`, preview: String(i) });
      }
      return { packed: pack(rows) };
    }
    if (method === 'forget') {
      this.resident = 0;
      return {};
    }
    throw new Error(`unexpected call ${method}`);
  }
}

/** Wait for the store's promise chains to settle. */
const settle = async (turns = 12) => {
  for (let i = 0; i < turns; i++) {
    await Promise.resolve();
  }
};

const events = () => ({
  repaints: 0,
  counts: [],
  rows() {
    this.repaints++;
  },
  count(container, count, complete) {
    this.counts.push([container, count, complete]);
  },
  incomplete() {},
  failed(thrown) {
    throw thrown;
  },
});

await check('a miss returns undefined, and the row arrives after the round trip', async () => {
  const engine = new FakeEngine(200);
  const seen = events();
  const store = new RowStore(engine, seen);

  store.grow(7, 0);
  await settle();

  assert.equal(store.rowAt(7, 0), undefined, 'the first ask is always a miss');
  await settle();

  const row = store.rowAt(7, 0);
  assert.ok(row, 'and the second is not');
  assert.equal(row.preview, '0');
  assert.equal(row.key, 'k0');
  assert.ok(seen.repaints > 0, 'and the page was told to repaint');
});

await check('one fetch serves a whole block', async () => {
  const engine = new FakeEngine(1000, 1000);
  const store = new RowStore(engine, events());

  store.grow(7, 0);
  await settle();
  store.rowAt(7, 0);
  await settle();

  const after = engine.calls.rows;
  for (let i = 0; i < BLOCK_ROWS; i++) {
    assert.ok(store.rowAt(7, i), `row ${i} is present`);
  }
  assert.equal(engine.calls.rows, after, 'no further calls for rows in the same block');

  store.rowAt(7, BLOCK_ROWS);
  await settle();
  assert.equal(engine.calls.rows, after + 1, 'and exactly one for the next block');
});

await check('an evicted container is rebuilt rather than shown as empty', async () => {
  // The bargain C36 struck: eviction costs work, never addressability. If this
  // regresses, a collapsed-then-scrolled tree paints blank rows and looks like
  // data loss.
  const engine = new FakeEngine(300, 300);
  const store = new RowStore(engine, events());

  store.grow(7, 0);
  await settle();
  assert.equal(store.extentOf(7).count, 300);

  engine.evict();
  const expansions = engine.calls.expand;

  store.rowAt(7, 260); // a block nothing has fetched yet
  await settle();

  assert.ok(engine.calls.expand > expansions, 'the store re-expanded');
  const row = store.rowAt(7, 260);
  assert.ok(row, 'and the row is there');
  assert.equal(row.preview, '260');
});

await check('growth stops once the target is covered', async () => {
  const engine = new FakeEngine(5_000_000, 10_000);
  const store = new RowStore(engine, events());

  store.grow(7, 100);
  await settle(40);

  assert.equal(engine.calls.expand, 1, 'one batch was enough for row 100');
  assert.equal(store.extentOf(7).count, 10_000);
  assert.equal(store.extentOf(7).complete, false);
});

await check('clearing discards answers to the file that was closed', async () => {
  const engine = new FakeEngine(200);
  const seen = events();
  const store = new RowStore(engine, seen);

  store.grow(7, 0);
  store.rowAt(7, 0);
  store.clear();
  await settle();

  assert.equal(store.rowAt(7, 0), undefined, 'nothing from the old file was kept');
  assert.equal(store.extentOf(7).count, 0);
});

/* ---------------------------------------------------------------- Search */

/** One instalment as the Worker posts it. */
const found = (search, rows, extra = {}) => ({
  kind: 'found',
  search,
  rows: Float64Array.from(rows),
  matches: extra.matches ?? rows.length,
  pending: extra.pending ?? 0,
  scanned: 0,
  total: 0,
  done: extra.done ?? false,
  limited: extra.limited ?? false,
});

await check('results accumulate across instalments', async () => {
  const search = new Search();
  search.begin();

  assert.equal(search.accept(found(1, [3, 9])), true);
  assert.equal(search.accept(found(1, [14], { matches: 3, done: true })), true);

  assert.equal(search.size, 3);
  assert.equal(search.matches, 3);
  assert.equal(search.scanning, false, 'the last instalment ended the scan');
});

await check('a superseded search is ignored, and a newer one takes over', async () => {
  // The property typing depends on: every keystroke starts a scan, and the one
  // it replaced keeps posting for a frame or two. Accepting those would splice
  // one search's results into another's list.
  const search = new Search();
  search.begin();
  search.accept(found(1, [1, 2, 3]));
  assert.equal(search.size, 3);

  search.begin(); // the next keystroke
  assert.equal(search.accept(found(1, [4])), false, 'the old scan is not ours');
  assert.equal(search.size, 0);

  assert.equal(search.accept(found(2, [8])), true, 'the new one is');
  assert.equal(search.size, 1);
  assert.equal(search.row, undefined, 'and nothing has been jumped to yet');
});

await check('a row that matches twice is two results but one mark', async () => {
  // "3 of 12" has to agree with pressing Enter twelve times, and a row can only
  // be painted once.
  const search = new Search();
  search.begin();
  search.accept(found(1, [5, 5, 9], { matches: 3, done: true }));

  assert.equal(search.size, 3, 'three results');
  assert.equal(search.mark(5), 'match');
  assert.equal(search.mark(9), 'match');
  assert.equal(search.mark(6), undefined);

  assert.equal(search.goTo(0), 5);
  assert.equal(search.mark(5), 'current');
  assert.equal(search.goTo(1), 5, 'the second hit is in the same row');
  assert.equal(search.goTo(2), 9);
  assert.equal(search.mark(5), 'match', 'which is no longer the current one');
});

await check('stepping wraps at both ends', async () => {
  const search = new Search();
  search.begin();
  search.accept(found(1, [2, 4, 6], { done: true }));

  assert.equal(search.goTo(0), 2);
  assert.equal(search.goTo(3), 2, 'past the last result comes back to the first');
  assert.equal(search.goTo(-1), 6, 'and back from the first is the last');
  assert.equal(search.at, 2);
});

await check('stepping an empty result set does nothing', async () => {
  const search = new Search();
  search.begin();
  search.accept(found(1, [], { matches: 0, done: true }));

  assert.equal(search.goTo(0), undefined);
  assert.equal(search.goTo(-1), undefined);
  assert.equal(search.at, -1);
});

await check('the status line never overstates what was found', async () => {
  const search = new Search();
  assert.equal(describeSearch(search, ''), '', 'an empty box says nothing');

  search.begin();
  assert.equal(describeSearch(search, 'x'), 'searching…');

  search.accept(found(1, [], { matches: 0, done: true }));
  assert.equal(describeSearch(search, 'x'), 'no matches');

  // A capped scan has a floor, not a total, and must print the `+`.
  const capped = new Search();
  capped.begin();
  capped.accept(found(2, [1, 2], { matches: 10_000, done: true, limited: true }));
  capped.goTo(0);
  assert.equal(describeSearch(capped, 'x'), '1 of 10,000+');

  // Matches with no row must be visible, or the count disagrees with the list.
  const partial = new Search();
  partial.begin();
  partial.accept(found(3, [1], { matches: 12, pending: 11, done: true }));
  partial.goTo(0);
  assert.equal(describeSearch(partial, 'x'), '1 of 12 · 11 unindexed');
});

console.log('\nleviathan viewer unit tests\n');
console.log(checks.join('\n'));
console.log(process.exitCode ? '\nFAILED\n' : '\nall good\n');
