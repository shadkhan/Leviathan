/**
 * Viewer page entry point — the tree, and everything that drives it.
 *
 * This thread renders. It does not parse, it does not read the file, and it
 * never holds more rows than fit on a screen. What lives here is the join
 * between three small pieces that each know one thing:
 *
 * - {@link Tree} knows the *shape* — which containers are open and therefore
 *   what row 4 812 907 is. It holds no row data at all.
 * - {@link RowStore} knows the *content* — a bounded cache of decoded blocks,
 *   and how to ask the Worker for one that is missing.
 * - {@link VirtualList} knows the *geometry* — how many rows exist, which are
 *   visible, and which DOM element to reuse for each.
 *
 * Keeping them apart is what makes the hard case boring: expanding a five
 * million element array changes one number in the tree, invalidates nothing in
 * the store, and moves no DOM at all.
 */

import { type FoundEvent, type Format, type WorkerEvent } from '../protocol/index.js';
import { RowBlock, type Row } from '../protocol/rows.js';
import { Engine, EngineError } from './engine.js';
import { VirtualList } from './list.js';
import { Search, describeSearch } from './search.js';
import { BLOCK_ROWS, RowStore } from './store.js';
import { Tree, type Branch, type Located } from './tree.js';

/** Look up a required element, failing loudly rather than at first null deref. */
function el<T extends HTMLElement>(id: string): T {
  const found = document.getElementById(id);
  if (!found) {
    throw new Error(`viewer.html is missing #${id} — the HTML and this bundle disagree.`);
  }
  return found as T;
}

const statusDot = el('status').querySelector<HTMLElement>('.dot');
const statusText = el('status-text');
const engineVersion = el('engine-version');

const fileInfo = el('file-info');
const fileName = el('file-name');
const fileFacts = el('file-facts');

const pick = el<HTMLButtonElement>('pick');
const pickEmpty = el<HTMLButtonElement>('pick-empty');
const filePicker = el<HTMLInputElement>('file');
const drop = el('drop');
const paste = el<HTMLTextAreaElement>('paste');
const empty = el('empty');

const crumbs = el('crumbs');
const indexing = el('indexing');
const progressFill = el('progress-fill');
const indexState = el('index-state');
const cancel = el<HTMLButtonElement>('cancel');

const findBar = el('find-bar');
const findInput = el<HTMLInputElement>('find-input');
const findStatus = el('find-status');
const findFilter = el<HTMLButtonElement>('find-filter');
const findPrev = el<HTMLButtonElement>('find-prev');
const findNext = el<HTMLButtonElement>('find-next');

const viewport = el('viewport');
const canvas = el('canvas');

const selectionInfo = el('selection');
const copyPath = el<HTMLButtonElement>('copy-path');
const copyValue = el<HTMLButtonElement>('copy-value');
const notice = el<HTMLOutputElement>('notice');

/**
 * Row height, taken from the stylesheet rather than duplicated here.
 *
 * The renderer positions every row by multiplying this; a stylesheet that
 * disagreed with it would put every row in the wrong place, which is a bug that
 * looks like a rendering glitch and is actually a constant in two files.
 */
const ROW_HEIGHT =
  Number.parseFloat(getComputedStyle(document.documentElement).getPropertyValue('--row-h')) || 22;

/**
 * How far ahead of the viewport a container is kept indexed.
 *
 * Two blocks: far enough that scrolling at speed reaches indexed rows, close
 * enough that opening a huge array does not index the whole thing (C39).
 */
const PREFETCH_ROWS = BLOCK_ROWS * 2;

/** Bytes of a value the clipboard will take before it is cut short. */
const COPY_LIMIT = 4 * 1024 * 1024;

function setStatus(state: 'pending' | 'ready' | 'failed', text: string): void {
  statusDot?.setAttribute('data-state', state);
  statusText.textContent = text;
}

function say(state: 'ok' | 'err' | '', text: string): void {
  notice.dataset['state'] = state;
  notice.textContent = text;
}

/** Render a thrown value in a way that names the layer that failed. */
function describe(thrown: unknown): string {
  if (thrown instanceof EngineError) {
    return thrown.cause === undefined ? thrown.message : `${thrown.message} (${thrown.cause})`;
  }
  return thrown instanceof Error ? thrown.message : String(thrown);
}

/** How each detected format reads to someone who just dropped a file. */
const FORMAT_LABEL: Record<Format, string> = {
  'single-document': 'JSON document',
  ndjson: 'NDJSON',
  empty: 'empty',
  unknown: 'not JSON',
};

/** Why indexing stopped early, said the way a user would say it. */
const STOPPED: Record<'malformed' | 'cancelled' | 'error', string> = {
  malformed: 'stopped at a syntax error',
  cancelled: 'stopped',
  error: 'unreadable',
};

const UNITS = ['B', 'kB', 'MB', 'GB', 'TB'];

function humanBytes(bytes: number): string {
  let value = bytes;
  let unit = 0;
  while (value >= 1000 && unit < UNITS.length - 1) {
    value /= 1000;
    unit++;
  }
  const digits = unit === 0 || value >= 100 ? 0 : 1;
  return `${value.toFixed(digits)} ${UNITS[unit]}`;
}

// ---------------------------------------------------------------- the parts

const worker = new Worker(new URL('worker.js', import.meta.url), {
  type: 'module',
  name: 'leviathan-engine',
});

const engine = new Engine(worker, onEvent);
const tree = new Tree();

const store = new RowStore(engine, {
  rows: () => {
    list.refresh();
  },
  count: (container, count, complete) => {
    const branch = tree.branchOf(container);
    if (branch) {
      tree.setCount(branch, count, complete);
      sync();
    }
  },
  incomplete: (container) => {
    const branch = tree.branchOf(container);
    say('err', `A container ends early — showing the ${branch?.count ?? 0} children found.`);
  },
  failed: (thrown) => {
    say('err', describe(thrown));
  },
});

const list = new VirtualList({
  viewport,
  canvas,
  rowHeight: ROW_HEIGHT,
  create: createRow,
  paint: paintRow,
});

/** The row the keyboard acts on. Held as (container, index), never as a flat
 * index: a flat index changes meaning whenever anything above it opens. */
interface Selection {
  branch: Branch;
  index: number;
}

let selection: Selection | undefined;

/** Whether tier-1 indexing is still running. */
let busy = false;

/** Find results and the position within them. See `search.ts`. */
const search = new Search();

/** Whether the tree shows only matching records. */
let filtering = true;

/**
 * Whether the filtered view is actually in effect.
 *
 * Filtering with nothing typed would show an empty tree, and filtering before
 * the first result arrives would blank a file the user is still looking at. So
 * the mode is a *preference*, and this is whether it currently applies.
 */
function filtered(): boolean {
  return filtering && search.matchedRows.length > 0;
}

/**
 * The real index of a row, given where the tree thinks it is.
 *
 * The tree — and everything built on it: sizes, scroll position, selection —
 * works in *visible* positions. The store, the gutter and the search marks work
 * in *record* positions. While filtering these differ for root rows only, and
 * this is the single place they are reconciled. Keeping the translation here
 * rather than inside `Tree` is what stops filtering from touching the geometry
 * that virtual scrolling depends on.
 */
function recordIndex(at: { branch: Branch; index: number }): number {
  if (at.branch.container !== null || !filtered()) {
    return at.index;
  }
  return search.matchedRows[at.index] ?? at.index;
}

/** The row at a tree position, from cache, translating the position first. */
function rowAtPosition(at: { branch: Branch; index: number }): Row | undefined {
  return store.rowAt(at.branch.container, recordIndex(at));
}

// ------------------------------------------------------------------- rows

interface Parts {
  gutter: HTMLElement;
  twisty: HTMLButtonElement;
  key: HTMLElement;
  value: HTMLElement;
  note: HTMLElement;
}

/**
 * The element parts of a row, keyed by the row element.
 *
 * A `WeakMap` rather than fields on the element, and a lookup rather than a
 * query: `paint` runs for every visible row on every frame of a scroll, and
 * `querySelector` in that loop is the kind of cost that only shows up on the
 * file the product exists for.
 */
const parts = new WeakMap<HTMLElement, Parts>();

function createRow(): HTMLElement {
  const row = document.createElement('div');
  row.className = 'tree-row';
  row.setAttribute('role', 'treeitem');

  const gutter = document.createElement('span');
  gutter.className = 'gutter';

  const twisty = document.createElement('button');
  twisty.type = 'button';
  twisty.className = 'twisty';
  twisty.tabIndex = -1;
  twisty.setAttribute('aria-hidden', 'true');

  const key = document.createElement('span');
  key.className = 'key';

  const value = document.createElement('span');
  value.className = 'val';

  const note = document.createElement('span');
  note.className = 'note';

  row.append(gutter, twisty, key, value, note);
  parts.set(row, { gutter, twisty, key, value, note });
  return row;
}

function paintRow(element: HTMLElement, flat: number): void {
  const part = parts.get(element);
  if (!part) {
    return;
  }

  const at = tree.locate(flat);
  const index = recordIndex(at);
  const row = store.rowAt(at.branch.container, index);

  element.dataset['flat'] = String(flat);
  element.id = `row-${flat}`;
  element.style.setProperty('--depth', String(at.depth));
  element.setAttribute('aria-level', String(at.depth + 1));
  element.setAttribute('aria-posinset', String(at.index + 1));
  element.setAttribute('aria-setsize', String(at.branch.complete ? at.branch.count : -1));

  const selected = isSelected(at);
  element.setAttribute('aria-selected', selected ? 'true' : 'false');
  if (selected) {
    viewport.setAttribute('aria-activedescendant', element.id);
  }

  // Only root rows can carry a match: a hit is a byte offset, and the table it
  // is resolved against is tier 1 (`rows_of` in the core). A match inside a
  // nested value therefore marks the record that contains it, which is the row
  // the user is looking for anyway.
  const mark = at.branch.container === null ? search.mark(index) : undefined;
  if (mark) {
    element.dataset['match'] = mark === 'current' ? 'current' : 'true';
  } else {
    delete element.dataset['match'];
  }

  part.gutter.textContent = index.toLocaleString();

  if (!row) {
    // The bytes have not arrived. The row still occupies its place, so nothing
    // moves when it does.
    element.dataset['loading'] = 'true';
    element.dataset['kind'] = 'pending';
    element.removeAttribute('aria-expanded');
    part.twisty.textContent = '';
    part.twisty.removeAttribute('title');
    part.key.textContent = '';
    part.value.textContent = '…';
    part.note.textContent = '';
    keepIndexed(at);
    return;
  }

  delete element.dataset['loading'];
  element.dataset['kind'] = row.kind;

  const open = row.expandable ? tree.branchOf(row.offset) : undefined;
  if (row.expandable) {
    element.setAttribute('aria-expanded', open ? 'true' : 'false');
    // U+2212 rather than a hyphen: a hyphen sits high and short next to a plus,
    // and the pair has to read as one control at 15 px.
    part.twisty.textContent = open ? '−' : '+';
    part.twisty.title = open ? 'Collapse' : 'Expand';
  } else {
    element.removeAttribute('aria-expanded');
    part.twisty.textContent = '';
    part.twisty.removeAttribute('title');
  }

  part.key.textContent = row.key === null ? '' : `${JSON.stringify(row.key)}:`;
  part.value.textContent = summarize(row);
  part.note.textContent = open && !open.complete ? 'indexing…' : countOf(row);

  keepIndexed(at);
}

/**
 * Keep the container a visible row belongs to indexed a little further ahead.
 *
 * The root grows on its own while tier 1 runs; an expanded container only grows
 * when someone asks, and the someone is this. Asking from `paint` means a
 * container is extended exactly when it is scrolled into, and never otherwise.
 */
function keepIndexed(at: Located): void {
  const container = at.branch.container;
  if (typeof container === 'number' && !at.branch.complete) {
    if (at.index + PREFETCH_ROWS >= at.branch.count) {
      store.grow(container, at.index + PREFETCH_ROWS);
    }
  }
}

/**
 * The one-line rendering of a value.
 *
 * For a container this is its first few fields — `{id: 0, level: "info", …}` —
 * because a row reading `{ 11 items }` tells you nothing about the record you
 * are looking at, and a tree you must expand before you can tell one row from
 * the next is a tree that has not helped yet. The count moves to the right-hand
 * note, where it is still visible but no longer the only thing there.
 */
function summarize(row: Row): string {
  if (row.kind === 'object' || row.kind === 'array') {
    const [open, close] = row.kind === 'object' ? ['{', '}'] : ['[', ']'];
    if (row.children === 0) {
      return `${open}${close}`;
    }
    const inner = row.truncated ? `${row.preview}, …` : row.preview;
    return `${open} ${inner} ${close}`;
  }
  return row.truncated ? `${row.preview}…` : row.preview;
}

/** The item count for a container, shown dimmed at the end of the row. */
function countOf(row: Row): string {
  if (row.kind !== 'object' && row.kind !== 'array') {
    return '';
  }
  if (row.children === 0) {
    return 'empty';
  }
  // An inexact count is the budget having run out, not a mystery — C33.
  const count = `${row.children.toLocaleString()}${row.childrenExact ? '' : '+'}`;
  return `${count} ${row.children === 1 ? 'item' : 'items'}`;
}

// -------------------------------------------------------------- structure

/** Root rows the engine has indexed, and whether it finished. */
let rootRows = 0;
let rootComplete = false;

/**
 * Tell the tree how many rows its root offers.
 *
 * Filtered, that is the number of matching records; unfiltered, everything
 * indexed so far. Nothing else in the tree knows the difference — which is the
 * point, because scroll geometry and expansion arithmetic have no business
 * caring whether a search is running.
 */
function applyRootCount(): void {
  if (filtered()) {
    tree.setCount(tree.root, search.matchedRows.length, !search.scanning);
  } else {
    tree.setCount(tree.root, rootRows, rootComplete);
  }
  viewport.dataset['filtered'] = filtered() ? 'true' : 'false';
}

/**
 * Close every open container.
 *
 * Needed whenever root positions *move*, because an open branch remembers which
 * child of the root it hangs from. Appending new matches never moves anything
 * already in the list, so this is not called as results stream in — only when
 * the filter is toggled or a new search replaces the old one.
 */
function collapseAll(): void {
  while (tree.root.children.length > 0) {
    for (const offset of tree.close(tree.root.children[0] as Branch)) {
      store.forget(offset);
    }
  }
  if (selection && selection.branch !== tree.root) {
    select(undefined);
  }
}

/** Push the model's current size into the list and refresh what is on screen. */
function sync(): void {
  applyRootCount();
  list.setCount(tree.size);
  list.refresh();
  renderCrumbs();
}

function isSelected(at: Located): boolean {
  return selection !== undefined && selection.branch === at.branch && selection.index === at.index;
}

/** Open or close the container at a flat row index. */
function toggle(flat: number): void {
  const at = tree.locate(flat);
  const row = store.rowAt(at.branch.container, recordIndex(at));
  if (!row?.expandable) {
    return;
  }

  const open = tree.branchOf(row.offset);
  if (open) {
    // Collapsing takes the subtree's expansions with it. Telling the engine is
    // courtesy, not correctness — a byte offset stays valid either way (C36).
    for (const offset of tree.close(open)) {
      store.forget(offset);
    }
    if (selection && !isReachable(selection.branch)) {
      select({ branch: at.branch, index: at.index });
    }
  } else {
    const extent = store.extentOf(row.offset);
    tree.open(at, row.offset, extent.count, extent.complete);
    store.grow(row.offset, PREFETCH_ROWS);
  }

  sync();
}

/** Whether a branch is still part of the tree, after a collapse elsewhere. */
function isReachable(branch: Branch): boolean {
  let node: Branch | null = branch;
  while (node.parent) {
    if (!node.parent.children.includes(node)) {
      return false;
    }
    node = node.parent;
  }
  return node === tree.root;
}

// -------------------------------------------------------------- selection

function select(next: Selection | undefined, reveal = true): void {
  selection = next;
  const enabled = next !== undefined;
  copyPath.disabled = !enabled;
  copyValue.disabled = !enabled;

  if (next && reveal) {
    list.reveal(tree.flatIndexOf(next.branch, next.index));
  }
  list.refresh();
  renderCrumbs();
  renderSelectionInfo();
}

/** Move the selection by whole rows, through the flat view of the tree. */
function move(delta: number): void {
  if (tree.size === 0) {
    return;
  }
  const from = selection ? tree.flatIndexOf(selection.branch, selection.index) : -1;
  const to = Math.max(0, Math.min(tree.size - 1, from + delta));
  select(tree.locate(to));
}

function selectFlat(flat: number): void {
  if (tree.size === 0) {
    return;
  }
  select(tree.locate(Math.max(0, Math.min(tree.size - 1, flat))));
}

/**
 * The row for a (container, index), from cache if it is there.
 *
 * Used by the parts of the UI that need a row they are not painting — the
 * breadcrumb and copy-path walk ancestors, which are usually cached because you
 * had to see a node to open it, and are fetched individually when they are not.
 */
async function rowFor(branch: Branch, index: number): Promise<Row | undefined> {
  const at = recordIndex({ branch, index });
  const cached = store.rowAt(branch.container, at);
  if (cached) {
    return cached;
  }
  const { packed } = await engine.call('rows', {
    container: branch.container,
    start: index,
    count: 1,
  });
  const block = new RowBlock(packed);
  return block.length === 0 ? undefined : block.row(0);
}

/** One step of a path: `.key`, `["odd key"]`, or `[7]`. */
function segment(row: Row | undefined, index: number): string {
  if (!row || row.key === null) {
    return `[${index}]`;
  }
  return /^[A-Za-z_$][\w$]*$/.test(row.key) ? `.${row.key}` : `[${JSON.stringify(row.key)}]`;
}

/** The chain of (branch, index) from the root down to a selection. */
function ancestry(of: Selection): Selection[] {
  const chain: Selection[] = [];
  let branch: Branch = of.branch;
  let index = of.index;
  for (;;) {
    chain.unshift({ branch, index });
    const parent = branch.parent;
    if (!parent) {
      return chain;
    }
    index = branch.indexInParent;
    branch = parent;
  }
}

/** The full path of a selection, fetching any ancestor rows not in cache. */
async function pathOf(of: Selection): Promise<string> {
  const steps = await Promise.all(
    ancestry(of).map(async (step) =>
      segment(await rowFor(step.branch, step.index), recordIndex(step)),
    ),
  );
  return `$${steps.join('')}`;
}

function renderCrumbs(): void {
  crumbs.replaceChildren();
  if (!selection) {
    return;
  }

  const chain = ancestry(selection);
  const root = document.createElement('span');
  root.className = 'sep';
  root.textContent = '$';
  crumbs.append(root);

  for (const [depth, step] of chain.entries()) {
    // Cache-only: the breadcrumb repaints on every arrow key, and a fetch per
    // keystroke to label a crumb the user can already see is a poor trade. A
    // missing row shows as its index, which is what it is.
    const label = segment(rowAtPosition(step), recordIndex(step));
    const last = depth === chain.length - 1;

    if (last) {
      const here = document.createElement('span');
      here.className = 'here';
      here.textContent = label;
      crumbs.append(here);
      continue;
    }

    const button = document.createElement('button');
    button.type = 'button';
    button.textContent = label;
    button.addEventListener('click', () => {
      select(step);
    });
    crumbs.append(button);
  }
}

function renderSelectionInfo(): void {
  selectionInfo.replaceChildren();
  if (!selection) {
    const hint = document.createElement('span');
    hint.className = 'hint';
    hint.textContent = 'Select a row to see its path and offset.';
    selectionInfo.append(hint);
    return;
  }

  const row = rowAtPosition(selection);
  const path = document.createElement('span');
  path.className = 'path';
  path.textContent = `$${ancestry(selection)
    .map((step) => segment(rowAtPosition(step), recordIndex(step)))
    .join('')}`;

  const facts = document.createElement('span');
  facts.className = 'facts';
  facts.textContent = row
    ? `${row.kind} · byte ${row.valueStart.toLocaleString()}${
        row.valueEnd === null ? '' : ` · ${humanBytes(row.valueEnd - row.valueStart)}`
      }`
    : 'loading…';

  selectionInfo.append(path, facts);
}

// ------------------------------------------------------------------ copy

async function toClipboard(text: string, what: string): Promise<void> {
  try {
    await navigator.clipboard.writeText(text);
    say('ok', `${what} copied — ${text.length.toLocaleString()} characters`);
  } catch (thrown) {
    say('err', `Could not copy: ${describe(thrown)}`);
  }
}

copyPath.addEventListener('click', () => {
  if (!selection) {
    return;
  }
  void pathOf(selection).then((path) => toClipboard(path, 'Path'));
});

copyValue.addEventListener('click', () => {
  void copySelectedValue();
});

/**
 * Copy the selected value's actual bytes, not its preview.
 *
 * The preview is truncated by design (C33), so copying it would silently hand
 * over the wrong thing. The Worker re-reads the value's byte range from the
 * file instead — bounded, because a single value can be larger than memory and
 * the clipboard is not the place to discover that.
 */
async function copySelectedValue(): Promise<void> {
  if (!selection) {
    return;
  }
  const row = await rowFor(selection.branch, selection.index);
  if (!row) {
    say('err', 'That row is not loaded yet.');
    return;
  }

  try {
    const { text, truncated } = await engine.call('text', {
      start: row.valueStart,
      end: row.valueEnd,
      limit: COPY_LIMIT,
    });
    await toClipboard(text, 'Value');
    if (truncated) {
      say('err', `Value copied, cut at ${humanBytes(COPY_LIMIT)}.`);
    }
  } catch (thrown) {
    say('err', describe(thrown));
  }
}

// ------------------------------------------------------------- interaction

viewport.addEventListener('mousedown', (event) => {
  const target = event.target as HTMLElement;
  const rowElement = target.closest<HTMLElement>('.tree-row');
  const flat = Number(rowElement?.dataset['flat']);
  if (!rowElement || Number.isNaN(flat)) {
    return;
  }

  if (target.classList.contains('twisty')) {
    event.preventDefault();
    toggle(flat);
    selectFlat(flat);
    return;
  }
  selectFlat(flat);
});

viewport.addEventListener('dblclick', (event) => {
  const rowElement = (event.target as HTMLElement).closest<HTMLElement>('.tree-row');
  const flat = Number(rowElement?.dataset['flat']);
  if (rowElement && !Number.isNaN(flat)) {
    toggle(flat);
  }
});

viewport.addEventListener('keydown', (event) => {
  if (event.altKey || event.ctrlKey || event.metaKey) {
    if ((event.ctrlKey || event.metaKey) && event.key.toLowerCase() === 'c') {
      event.preventDefault();
      if (event.shiftKey) {
        if (selection) {
          void pathOf(selection).then((path) => toClipboard(path, 'Path'));
        }
      } else {
        void copySelectedValue();
      }
    }
    return;
  }

  const flat = selection ? tree.flatIndexOf(selection.branch, selection.index) : 0;

  switch (event.key) {
    case 'ArrowDown':
      move(1);
      break;
    case 'ArrowUp':
      move(-1);
      break;
    case 'PageDown':
      move(list.pageSize);
      break;
    case 'PageUp':
      move(-list.pageSize);
      break;
    case 'Home':
      selectFlat(0);
      break;
    case 'End':
      selectFlat(tree.size - 1);
      break;
    case 'ArrowRight': {
      if (!selection) {
        selectFlat(0);
        break;
      }
      const row = store.rowAt(selection.branch.container, selection.index);
      if (row?.expandable && !tree.branchOf(row.offset)) {
        toggle(flat);
      } else {
        move(1);
      }
      break;
    }
    case 'ArrowLeft': {
      if (!selection) {
        selectFlat(0);
        break;
      }
      const row = store.rowAt(selection.branch.container, selection.index);
      if (row?.expandable && tree.branchOf(row.offset)) {
        toggle(flat);
      } else if (selection.branch.parent) {
        select({ branch: selection.branch.parent, index: selection.branch.indexInParent });
      }
      break;
    }
    case 'Enter':
    case ' ':
      toggle(flat);
      break;
    default:
      return;
  }

  event.preventDefault();
});

// ------------------------------------------------------------------- find

/** Turn filtering on or off, and put the tree back in a consistent state. */
function setFiltering(on: boolean): void {
  if (filtering === on) {
    return;
  }
  filtering = on;
  findFilter.setAttribute('aria-pressed', on ? 'true' : 'false');
  // Root positions change meaning, so anything open is hanging from an index
  // that no longer refers to what it did.
  collapseAll();
  list.reset();
  sync();
}

/** Throw away every result and stop the Worker scanning. */
function resetSearch(): void {
  const wasFiltered = filtered();
  search.reset();
  if (wasFiltered) {
    // The filtered view is emptying, so every open branch's position is stale.
    collapseAll();
    list.reset();
  }
  sync();
  renderFind();
}

/** Start scanning for what is in the box, or clear if it is empty. */
function runSearch(): void {
  const needle = findInput.value;

  if (needle.length === 0) {
    resetSearch();
    void engine.call('findStop', {}).catch(() => {
      // Nothing to stop is the common case and not worth saying.
    });
    return;
  }

  search.begin();
  renderFind();
  list.refresh();
  // Case-insensitive always, for now: it is what a find box is expected to do,
  // and a toggle is a control to explain in a toolbar that has to stay legible.
  engine.call('find', { needle, caseSensitive: false }).catch((thrown: unknown) => {
    search.fail();
    say('err', describe(thrown));
    renderFind();
  });
}

/** One instalment of results from the Worker. */
function onFound(event: FoundEvent): void {
  const first = search.size === 0;
  if (!search.accept(event)) {
    return; // A superseded search still posting. Not ours.
  }

  if (event.error) {
    say('err', describe(new EngineError(event.error)));
  }

  // Rows appended to the filtered view make the tree taller; nothing already in
  // it moves, so open branches stay valid and no reset is needed.
  sync();

  // Land on the first result as soon as there is one, the way a browser's find
  // does — waiting for a 500 MB scan to finish before moving is the behaviour
  // that makes people think a search is broken.
  if (first && search.size > 0) {
    goToMatch(0);
  } else {
    renderFind();
    list.refresh();
  }
}

/** Select and reveal the `n`-th result, wrapping at both ends. */
function goToMatch(n: number): void {
  const row = search.goTo(n);
  if (row === undefined) {
    return;
  }
  // `row` is a record index; the tree is addressed in visible positions, and
  // while filtering those are not the same number.
  const position = filtered() ? search.positionOf(row) : row;
  select({ branch: tree.root, index: position < 0 ? row : position });
  renderFind();
}

function renderFind(): void {
  findInput.dataset['scanning'] = search.scanning ? 'true' : 'false';
  findPrev.disabled = search.size === 0;
  findNext.disabled = search.size === 0;

  findStatus.textContent = describeSearch(search, findInput.value);
  if (findInput.value.length > 0 && search.matches === 0 && !search.scanning) {
    findStatus.dataset['state'] = 'none';
  } else {
    delete findStatus.dataset['state'];
  }
}

let findTimer: number | undefined;
findInput.addEventListener('input', () => {
  clearTimeout(findTimer);
  // Long enough that typing a word is one scan rather than five, short enough
  // that it feels like it reacted to the last keystroke.
  findTimer = self.setTimeout(runSearch, 200);
});

findInput.addEventListener('keydown', (event) => {
  if (event.key === 'Enter') {
    event.preventDefault();
    clearTimeout(findTimer);
    if (search.size === 0) {
      runSearch();
    } else {
      goToMatch(search.at + (event.shiftKey ? -1 : 1));
    }
    return;
  }
  if (event.key === 'Escape') {
    event.preventDefault();
    findInput.value = '';
    resetSearch();
    void engine.call('findStop', {}).catch(() => {
      // Stopping a scan that has already finished is not a failure.
    });
    viewport.focus();
  }
});

findFilter.addEventListener('click', () => {
  setFiltering(!filtering);
});

findNext.addEventListener('click', () => {
  goToMatch(search.at + 1);
});
findPrev.addEventListener('click', () => {
  goToMatch(search.at - 1);
});

// Ctrl/Cmd+F anywhere on the page, including from inside the tree — the whole
// point is that it is reachable without leaving the keyboard.
document.addEventListener('keydown', (event) => {
  if (findBar.hidden) {
    return;
  }
  if ((event.ctrlKey || event.metaKey) && event.key.toLowerCase() === 'f') {
    event.preventDefault();
    findInput.focus();
    findInput.select();
    return;
  }
  if (event.altKey && event.key.toLowerCase() === 'f') {
    event.preventDefault();
    setFiltering(!filtering);
  }
});

// ------------------------------------------------------------------ files

/** Hand a file to the engine and show its root. */
async function openFile(source: File): Promise<void> {
  say('', `opening ${source.name}…`);
  selection = undefined;
  copyPath.disabled = true;
  copyValue.disabled = true;
  tree.reset();
  store.clear();
  list.reset();
  list.setCount(0);
  findInput.value = '';
  resetSearch();
  renderCrumbs();
  renderSelectionInfo();

  try {
    const { format, size } = await engine.call('open', { file: source });

    fileInfo.hidden = false;
    fileName.textContent = source.name;
    fileFacts.textContent = `${humanBytes(size)} · ${FORMAT_LABEL[format]}`;
    empty.hidden = true;
    viewport.hidden = false;
    indexing.hidden = false;
    findBar.hidden = false;
    viewport.focus();
    say('', '');

    if (format === 'unknown' || format === 'empty') {
      say('err', `Nothing to show — this file is ${FORMAT_LABEL[format]}.`);
    }
  } catch (thrown) {
    say('err', describe(thrown));
  }
}

function onEvent(event: WorkerEvent): void {
  if (event.kind === 'ready') {
    setStatus('ready', 'Engine ready');
    engineVersion.textContent = `engine ${event.core} · protocol ${event.protocol}`;
    return;
  }

  if (event.kind === 'fatal') {
    setStatus('failed', 'Engine failed to start');
    say('err', describe(new EngineError(event.error)));
    return;
  }

  if (event.kind === 'found') {
    onFound(event);
    return;
  }

  const percent = event.total === 0 ? 100 : (event.consumed / event.total) * 100;
  progressFill.style.width = `${Math.min(100, percent).toFixed(1)}%`;
  busy = !event.done;
  cancel.disabled = event.done;
  viewport.setAttribute('aria-busy', busy ? 'true' : 'false');

  const rows = `${event.rows.toLocaleString()} rows`;
  if (!event.done) {
    progressFill.dataset['state'] = '';
    indexState.textContent = `${humanBytes(event.consumed)} · ${rows}`;
  } else if (event.stopped) {
    progressFill.dataset['state'] = 'stopped';
    indexState.textContent = `${STOPPED[event.stopped]} · ${rows}`;
    if (event.error) {
      say('err', describe(new EngineError(event.error)));
    }
  } else {
    progressFill.dataset['state'] = 'done';
    progressFill.style.width = '100%';
    indexState.textContent = rows;
  }

  // Tier 1 only ever appends, so a growing root is a taller scrollbar and
  // nothing else — no row on screen moves, and nothing cached is invalidated.
  store.noteRoot(event.rows, event.done);
  rootRows = event.rows;
  rootComplete = event.done;
  sync();
}

// Confirms the loaded `.wasm` matches this bundle. Also the first message sent,
// which is what triggers instantiation in the Worker.
engine.checkVersion().catch((thrown: unknown) => {
  setStatus('failed', 'Engine failed to start');
  say('err', describe(thrown));
});

cancel.addEventListener('click', () => {
  cancel.disabled = true;
  void engine.call('cancel', {}).catch(() => {
    // The final progress event reports the outcome; nothing to add here.
  });
});

for (const button of [pick, pickEmpty]) {
  button.addEventListener('click', () => {
    filePicker.click();
  });
}

filePicker.addEventListener('change', () => {
  const chosen = filePicker.files?.[0];
  if (chosen) {
    void openFile(chosen);
  }
});

drop.addEventListener('click', (event) => {
  if (event.target !== pickEmpty) {
    filePicker.click();
  }
});

drop.addEventListener('keydown', (event) => {
  if (event.key === 'Enter' || event.key === ' ') {
    event.preventDefault();
    filePicker.click();
  }
});

// Drops land anywhere on the page, not just on the empty state: once a file is
// open the drop zone is not on screen, and "drag another file in" is the most
// natural way to open the next one.
document.addEventListener('dragover', (event) => {
  event.preventDefault();
  drop.dataset['over'] = 'true';
});

for (const type of ['dragleave', 'drop'] as const) {
  document.addEventListener(type, () => {
    delete drop.dataset['over'];
  });
}

document.addEventListener('drop', (event) => {
  event.preventDefault();
  const dropped = event.dataTransfer?.files[0];
  if (dropped) {
    void openFile(dropped);
  }
});

// Pasted text becomes a `File` and takes exactly the same path as a dropped
// one. Not a shortcut — a second entry point with its own handling would be a
// second set of bugs, and this way paste is tested by everything file is.
// Debounced so typing does not open a document per keystroke.
let pasteTimer: number | undefined;
paste.addEventListener('input', () => {
  clearTimeout(pasteTimer);
  pasteTimer = self.setTimeout(() => {
    const text = paste.value;
    if (text.length > 0) {
      void openFile(new File([text], 'pasted.json', { type: 'application/json' }));
    }
  }, 150);
});
