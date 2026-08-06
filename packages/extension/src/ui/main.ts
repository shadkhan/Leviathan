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

import {
  searchModeOf,
  type ExportFormat,
  type FoundEvent,
  // Aliased: the DOM has a global `ProgressEvent`, and an unqualified reference
  // silently resolves to that one — which type-checks until it does not.
  type ProgressEvent as IndexProgress,
  type Format,
  type SearchMode,
  type WorkerEvent,
} from "../protocol/index.js";
import { RowBlock, type Row } from "../protocol/rows.js";
import { Engine, EngineError } from "./engine.js";
import { VirtualList } from "./list.js";
import { Search, describeSearch } from "./search.js";
import { BLOCK_ROWS, RowStore } from "./store.js";
import { parsePath, type Step } from "./path.js";
import { Tree, type Branch, type Located } from "./tree.js";

/** Look up a required element, failing loudly rather than at first null deref. */
function el<T extends HTMLElement>(id: string): T {
  const found = document.getElementById(id);
  if (!found) {
    throw new Error(
      `viewer.html is missing #${id} — the HTML and this bundle disagree.`,
    );
  }
  return found as T;
}

const statusDot = el("status").querySelector<HTMLElement>(".dot");
const statusText = el("status-text");
const engineVersion = el("engine-version");

const fileInfo = el("file-info");
const fileName = el("file-name");
const fileFacts = el("file-facts");

const pick = el<HTMLButtonElement>("pick");
const pickEmpty = el<HTMLButtonElement>("pick-empty");
const filePicker = el<HTMLInputElement>("file");
const pickFolder = el<HTMLButtonElement>("pick-folder");
const folderPicker = el<HTMLInputElement>("folder");
const fileList = el("file-list");
const drop = el("drop");
const paste = el<HTMLTextAreaElement>("paste");
const empty = el("empty");

const crumbs = el("crumbs");
const indexing = el("indexing");
const progressFill = el("progress-fill");
const indexState = el("index-state");
const cancel = el<HTMLButtonElement>("cancel");

const gotoBar = el("goto-bar");
const gotoInput = el<HTMLInputElement>("goto-input");
const collapseAllButton = el<HTMLButtonElement>("collapse-all");
const validateButton = el<HTMLButtonElement>("validate");
const pickSchema = el<HTMLButtonElement>("pick-schema");
const schemaFile = el<HTMLInputElement>("schema-file");
const problems = el("problems");
const problemsTitle = el("problems-title");
const problemsState = el("problems-state");
const problemsClose = el<HTMLButtonElement>("problems-close");
const problemsList = el("problems-list");
const dedupButton = el<HTMLButtonElement>("dedup");
const exportButton = el<HTMLButtonElement>("export");
const exportFormat = el<HTMLSelectElement>("export-format");
const findBar = el("find-bar");
const findInput = el<HTMLInputElement>("find-input");
const findStatus = el("find-status");
const findFilter = el<HTMLButtonElement>("find-filter");
const findPrev = el<HTMLButtonElement>("find-prev");
const findNext = el<HTMLButtonElement>("find-next");

const viewport = el("viewport");
const canvas = el("canvas");

const usageInfo = el("usage");
const selectionInfo = el("selection");
const copyPath = el<HTMLButtonElement>("copy-path");
const copyValue = el<HTMLButtonElement>("copy-value");
const notice = el<HTMLOutputElement>("notice");

/**
 * Row height, taken from the stylesheet rather than duplicated here.
 *
 * The renderer positions every row by multiplying this; a stylesheet that
 * disagreed with it would put every row in the wrong place, which is a bug that
 * looks like a rendering glitch and is actually a constant in two files.
 */
const ROW_HEIGHT =
  Number.parseFloat(
    getComputedStyle(document.documentElement).getPropertyValue("--row-h"),
  ) || 22;

/**
 * How far ahead of the viewport a container is kept indexed.
 *
 * Two blocks: far enough that scrolling at speed reaches indexed rows, close
 * enough that opening a huge array does not index the whole thing (C39).
 */
const PREFETCH_ROWS = BLOCK_ROWS * 2;

/** Bytes of a value the clipboard will take before it is cut short. */
const COPY_LIMIT = 4 * 1024 * 1024;

function setStatus(state: "pending" | "ready" | "failed", text: string): void {
  statusDot?.setAttribute("data-state", state);
  statusText.textContent = text;
}

function say(state: "ok" | "err" | "", text: string): void {
  notice.dataset["state"] = state;
  notice.textContent = text;
}

/** Render a thrown value in a way that names the layer that failed. */
function describe(thrown: unknown): string {
  if (thrown instanceof EngineError) {
    return thrown.cause === undefined
      ? thrown.message
      : `${thrown.message} (${thrown.cause})`;
  }
  return thrown instanceof Error ? thrown.message : String(thrown);
}

/** How each detected format reads to someone who just dropped a file. */
const FORMAT_LABEL: Record<Format, string> = {
  "single-document": "JSON document",
  ndjson: "NDJSON",
  empty: "empty",
  unknown: "not JSON",
};

/** Why indexing stopped early, said the way a user would say it. */
const STOPPED: Record<
  "malformed" | "cancelled" | "error" | "exhausted",
  string
> = {
  malformed: "stopped at a syntax error",
  cancelled: "stopped",
  error: "unreadable",
  exhausted: "stopped — index too large",
};

/**
 * How much index a 32-bit engine can realistically hold, in bytes.
 *
 * WebAssembly's address space is 4 GiB and the table is not the only thing in
 * it; a flat array of numbers was measured needing 800 MB of table and 2.15 GB
 * of linear memory for a 1 GB file, and failing outright at 2.5 GB. This is the
 * line at which it is worth saying something before the user has waited.
 */
const INDEX_CEILING = 1_400_000_000;

/** Whether the shape warning has already been given for this file. */
let warnedAboutShape = false;

/**
 * Group a non-negative integer's digits: `1772686` → `1,772,686`.
 *
 * Hand-rolled rather than `toLocaleString`, which builds or consults an `Intl`
 * formatter and is roughly an order of magnitude slower. That is irrelevant
 * anywhere it is called once, and it is not irrelevant in {@link paintRow},
 * which runs for every visible row of every frame — fifty-odd rows at sixty
 * frames a second (C53).
 */
function grouped(value: number): string {
  const digits = String(value);
  if (digits.length <= 3) {
    return digits;
  }
  let out = "";
  let cut = digits.length;
  while (cut > 3) {
    cut -= 3;
    out = `,${digits.slice(cut, cut + 3)}${out}`;
  }
  return digits.slice(0, cut) + out;
}

const UNITS = ["B", "kB", "MB", "GB", "TB"];

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

const worker = new Worker(new URL("worker.js", import.meta.url), {
  type: "module",
  name: "leviathan-engine",
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
    say(
      "err",
      `A container ends early — showing the ${branch?.count ?? 0} children found.`,
    );
  },
  failed: (thrown) => {
    say("err", describe(thrown));
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
  /**
   * What this element was last painted with.
   *
   * Compared against here rather than read back from the DOM: a scroll repaints
   * every visible row each frame, but most of them are unchanged, and assigning
   * `textContent` invalidates style whether or not the string differs. Holding
   * the previous values in JS turns "is this the same?" into a string compare
   * instead of a DOM read followed by a write.
   */
  last: Record<string, string>;
}

/** Assign only when the value actually changed. */
function set(
  part: Parts,
  slot: string,
  value: string,
  apply: (v: string) => void,
): void {
  if (part.last[slot] !== value) {
    part.last[slot] = value;
    apply(value);
  }
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
  const row = document.createElement("div");
  row.className = "tree-row";
  row.setAttribute("role", "treeitem");

  const gutter = document.createElement("span");
  gutter.className = "gutter";

  const twisty = document.createElement("button");
  twisty.type = "button";
  twisty.className = "twisty";
  twisty.tabIndex = -1;
  twisty.setAttribute("aria-hidden", "true");

  const key = document.createElement("span");
  key.className = "key";

  const value = document.createElement("span");
  value.className = "val";

  const note = document.createElement("span");
  note.className = "note";

  row.append(gutter, twisty, key, value, note);
  parts.set(row, { gutter, twisty, key, value, note, last: {} });
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

  const flatText = String(flat);
  set(part, "flat", flatText, (v) => {
    element.dataset["flat"] = v;
    element.id = `row-${v}`;
  });
  set(part, "depth", String(at.depth), (v) => {
    element.style.setProperty("--depth", v);
    element.setAttribute("aria-level", String(at.depth + 1));
  });
  set(part, "posinset", String(at.index + 1), (v) =>
    element.setAttribute("aria-posinset", v),
  );
  set(part, "setsize", String(at.branch.complete ? at.branch.count : -1), (v) =>
    element.setAttribute("aria-setsize", v),
  );

  const selected = isSelected(at);
  set(part, "selected", selected ? "true" : "false", (v) =>
    element.setAttribute("aria-selected", v),
  );
  if (selected) {
    viewport.setAttribute("aria-activedescendant", element.id);
  }

  // Only root rows can carry a match: a hit is a byte offset, and the table it
  // is resolved against is tier 1 (`rows_of` in the core). A match inside a
  // nested value therefore marks the record that contains it, which is the row
  // the user is looking for anyway.
  set(
    part,
    "problem",
    at.branch.container === null && problemRows.has(index) ? "true" : "",
    (v) => {
      if (v === "") delete element.dataset["problem"];
      else element.dataset["problem"] = v;
    },
  );

  const mark = at.branch.container === null ? search.mark(index) : undefined;
  set(part, "match", mark ?? "", (v) => {
    if (v === "") {
      delete element.dataset["match"];
    } else {
      element.dataset["match"] = v === "current" ? "current" : "true";
    }
  });

  set(part, "gutter", grouped(index), (v) => {
    part.gutter.textContent = v;
  });

  if (!row) {
    // The bytes have not arrived. The row still occupies its place, so nothing
    // moves when it does.
    set(part, "kind", "pending", (v) => {
      element.dataset["loading"] = "true";
      element.dataset["kind"] = v;
      element.removeAttribute("aria-expanded");
    });
    set(part, "twisty", "", (v) => {
      part.twisty.textContent = v;
      part.twisty.removeAttribute("title");
    });
    set(part, "key", "", (v) => {
      part.key.textContent = v;
    });
    set(part, "value", "…", (v) => {
      part.value.textContent = v;
    });
    set(part, "note", "", (v) => {
      part.note.textContent = v;
    });
    keepIndexed(at);
    return;
  }

  const open = row.expandable ? tree.branchOf(row.offset) : undefined;

  set(part, "kind", row.kind, (v) => {
    delete element.dataset["loading"];
    element.dataset["kind"] = v;
  });
  set(
    part,
    "expanded",
    row.expandable ? (open ? "true" : "false") : "",
    (v) => {
      if (v === "") {
        element.removeAttribute("aria-expanded");
      } else {
        element.setAttribute("aria-expanded", v);
      }
    },
  );
  // U+2212 rather than a hyphen: a hyphen sits high and short next to a plus,
  // and the pair has to read as one control at 15 px.
  set(part, "twisty", row.expandable ? (open ? "−" : "+") : "", (v) => {
    part.twisty.textContent = v;
    if (v === "") {
      part.twisty.removeAttribute("title");
    } else {
      part.twisty.title = v === "−" ? "Collapse" : "Expand";
    }
  });

  set(
    part,
    "key",
    row.key === null ? "" : `${JSON.stringify(row.key)}:`,
    (v) => {
      part.key.textContent = v;
    },
  );
  set(part, "value", summarize(row), (v) => {
    part.value.textContent = v;
  });
  set(
    part,
    "note",
    open && !open.complete ? "indexing…" : countOf(row),
    (v) => {
      part.note.textContent = v;
    },
  );

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
  if (typeof container === "number" && !at.branch.complete) {
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
  if (row.kind === "object" || row.kind === "array") {
    const [open, close] = row.kind === "object" ? ["{", "}"] : ["[", "]"];
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
  if (row.kind !== "object" && row.kind !== "array") {
    return "";
  }
  if (row.children === 0) {
    return "empty";
  }
  // An inexact count is the budget having run out, not a mystery — C33.
  const count = `${grouped(row.children)}${row.childrenExact ? "" : "+"}`;
  return `${count} ${row.children === 1 ? "item" : "items"}`;
}

// -------------------------------------------------------------- structure

/** Root rows the engine has indexed, and whether it finished. */
let rootRows = 0;
let rootComplete = false;

/** The size of the open file, for the memory readout's ratio. */
let fileBytes = 0;

/**
 * Show what the engine occupies (requirement 9).
 *
 * Both numbers are the engine's own: the index it built, and the linear memory
 * the browser actually reserved for it. The ratio is the claim worth making —
 * "14.2 MB for a 500 MB file" says more than either number alone, and it is the
 * whole memory argument in six characters.
 */
function renderUsage(usage: { index: number; heap: number }): void {
  usageInfo.hidden = false;
  const share =
    fileBytes > 0
      ? ` · ${((usage.index / fileBytes) * 100).toFixed(1)}% of file`
      : "";
  usageInfo.textContent = `index ${humanBytes(usage.index)} · heap ${humanBytes(usage.heap)}${share}`;
  usageInfo.title =
    `Index: ${usage.index.toLocaleString()} bytes of node offsets, tier 1 plus resident expansions.\n` +
    `Heap: ${usage.heap.toLocaleString()} bytes of WASM linear memory — the engine's real footprint.\n` +
    `The file itself is never held in memory; the engine reads byte ranges from it on demand.`;
}

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
  viewport.dataset["filtered"] = filtered() ? "true" : "false";
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
  return (
    selection !== undefined &&
    selection.branch === at.branch &&
    selection.index === at.index
  );
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
  const from = selection
    ? tree.flatIndexOf(selection.branch, selection.index)
    : -1;
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
  const { packed } = await engine.call("rows", {
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
  return /^[A-Za-z_$][\w$]*$/.test(row.key)
    ? `.${row.key}`
    : `[${JSON.stringify(row.key)}]`;
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
  return `$${steps.join("")}`;
}

function renderCrumbs(): void {
  crumbs.replaceChildren();
  if (!selection) {
    return;
  }

  const chain = ancestry(selection);
  const root = document.createElement("span");
  root.className = "sep";
  root.textContent = "$";
  crumbs.append(root);

  for (const [depth, step] of chain.entries()) {
    // Cache-only: the breadcrumb repaints on every arrow key, and a fetch per
    // keystroke to label a crumb the user can already see is a poor trade. A
    // missing row shows as its index, which is what it is.
    const label = segment(rowAtPosition(step), recordIndex(step));
    const last = depth === chain.length - 1;

    if (last) {
      const here = document.createElement("span");
      here.className = "here";
      here.textContent = label;
      crumbs.append(here);
      continue;
    }

    const button = document.createElement("button");
    button.type = "button";
    button.textContent = label;
    button.addEventListener("click", () => {
      select(step);
    });
    crumbs.append(button);
  }
}

function renderSelectionInfo(): void {
  selectionInfo.replaceChildren();
  if (!selection) {
    const hint = document.createElement("span");
    hint.className = "hint";
    hint.textContent = "Select a row to see its path and offset.";
    selectionInfo.append(hint);
    return;
  }

  const row = rowAtPosition(selection);
  const path = document.createElement("span");
  path.className = "path";
  path.textContent = `$${ancestry(selection)
    .map((step) => segment(rowAtPosition(step), recordIndex(step)))
    .join("")}`;

  const facts = document.createElement("span");
  facts.className = "facts";
  facts.textContent = row
    ? `${row.kind} · byte ${row.valueStart.toLocaleString()}${
        row.valueEnd === null
          ? ""
          : ` · ${humanBytes(row.valueEnd - row.valueStart)}`
      }`
    : "loading…";

  selectionInfo.append(path, facts);
}

// ------------------------------------------------------------------ copy

async function toClipboard(text: string, what: string): Promise<void> {
  try {
    await navigator.clipboard.writeText(text);
    say("ok", `${what} copied — ${text.length.toLocaleString()} characters`);
  } catch (thrown) {
    say("err", `Could not copy: ${describe(thrown)}`);
  }
}

copyPath.addEventListener("click", () => {
  if (!selection) {
    return;
  }
  void pathOf(selection).then((path) => toClipboard(path, "Path"));
});

copyValue.addEventListener("click", () => {
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
    say("err", "That row is not loaded yet.");
    return;
  }

  try {
    const { text, truncated } = await engine.call("text", {
      start: row.valueStart,
      end: row.valueEnd,
      limit: COPY_LIMIT,
    });
    await toClipboard(text, "Value");
    if (truncated) {
      say("err", `Value copied, cut at ${humanBytes(COPY_LIMIT)}.`);
    }
  } catch (thrown) {
    say("err", describe(thrown));
  }
}

// ------------------------------------------------------------- interaction

viewport.addEventListener("mousedown", (event) => {
  const target = event.target as HTMLElement;
  const rowElement = target.closest<HTMLElement>(".tree-row");
  const flat = Number(rowElement?.dataset["flat"]);
  if (!rowElement || Number.isNaN(flat)) {
    return;
  }

  if (target.classList.contains("twisty")) {
    event.preventDefault();
    toggle(flat);
    selectFlat(flat);
    return;
  }
  selectFlat(flat);
});

viewport.addEventListener("dblclick", (event) => {
  const rowElement = (event.target as HTMLElement).closest<HTMLElement>(
    ".tree-row",
  );
  const flat = Number(rowElement?.dataset["flat"]);
  if (rowElement && !Number.isNaN(flat)) {
    toggle(flat);
  }
});

viewport.addEventListener("keydown", (event) => {
  if (event.altKey || event.ctrlKey || event.metaKey) {
    if ((event.ctrlKey || event.metaKey) && event.key.toLowerCase() === "c") {
      event.preventDefault();
      if (event.shiftKey) {
        if (selection) {
          void pathOf(selection).then((path) => toClipboard(path, "Path"));
        }
      } else {
        void copySelectedValue();
      }
    }
    return;
  }

  const flat = selection
    ? tree.flatIndexOf(selection.branch, selection.index)
    : 0;

  switch (event.key) {
    case "ArrowDown":
      move(1);
      break;
    case "ArrowUp":
      move(-1);
      break;
    case "PageDown":
      move(list.pageSize);
      break;
    case "PageUp":
      move(-list.pageSize);
      break;
    case "Home":
      selectFlat(0);
      break;
    case "End":
      selectFlat(tree.size - 1);
      break;
    case "ArrowRight": {
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
    case "ArrowLeft": {
      if (!selection) {
        selectFlat(0);
        break;
      }
      const row = store.rowAt(selection.branch.container, selection.index);
      if (row?.expandable && tree.branchOf(row.offset)) {
        toggle(flat);
      } else if (selection.branch.parent) {
        select({
          branch: selection.branch.parent,
          index: selection.branch.indexInParent,
        });
      }
      break;
    }
    case "Enter":
    case " ":
      toggle(flat);
      break;
    default:
      return;
  }

  event.preventDefault();
});

// ------------------------------------------------------------ validation

/** Rows the validator objected to, for painting. */
let problemRows = new Set<number>();

/** Which pass the UI is listening to. Same discipline as search (C48). */
let validating = 0;

/**
 * Which pass the problems panel is currently reporting.
 *
 * Three passes share one panel — well-formedness, schema, duplicates — because
 * all three produce findings with a place in the file. They do *not* share
 * wording: "No syntax errors" over a duplicate-key run would be a true sentence
 * about a question nobody asked.
 */
let passKind: "validate" | "schema" | "dedup" = "validate";

/** How many problems the list will hold before it stops growing. */
const PROBLEM_ROWS = 500;

function resetProblems(): void {
  problemRows = new Set();
  problemsList.replaceChildren();
  problems.hidden = true;
}

/** One instalment of validation results. */
function onValidated(event: Extract<WorkerEvent, { kind: "validated" }>): void {
  if (event.pass < validating) {
    return; // A superseded pass still reporting.
  }
  validating = event.pass;
  problems.hidden = false;

  if (event.error) {
    say("err", describe(new EngineError(event.error)));
  }

  for (const problem of event.problems) {
    if (problem.row !== null) {
      problemRows.add(problem.row);
    }
    if (problemsList.childElementCount >= PROBLEM_ROWS) {
      continue;
    }

    const item = document.createElement("li");
    const button = document.createElement("button");
    button.type = "button";

    const where = document.createElement("span");
    where.className = "where";
    // A duplicate is a fact about two byte offsets and has no line of its own,
    // so it reports one rather than inventing `0:0`.
    where.textContent =
      problem.line === 0
        ? `@${grouped(problem.offset)}`
        : `${grouped(problem.line)}:${grouped(problem.column)}`;

    const what = document.createElement("span");
    what.className = "what";
    what.textContent = problem.message;

    button.append(where, what);
    button.addEventListener("click", () => {
      // Requirement 8: an error location you can go to. The row was resolved
      // by the engine, where the index is; if the byte fell before the first
      // row there is nothing to select, and the offset is still shown.
      if (problem.row === null) {
        say("err", `byte ${grouped(problem.offset)} is before the first row`);
        return;
      }
      setFiltering(false);
      select({ branch: tree.root, index: problem.row });
      viewport.focus();
    });

    item.append(button);
    problemsList.append(item);
  }

  const checked =
    event.bytes > 0 ? Math.round((event.checked / event.bytes) * 100) : 100;
  const capped =
    event.total > problemsList.childElementCount ? " (first 500 shown)" : "";
  const nothing = passKind === "dedup" ? "No duplicates" : "No syntax errors";
  const some =
    passKind === "dedup"
      ? `${grouped(event.total)} ${event.total === 1 ? "duplicate" : "duplicates"}`
      : `${grouped(event.total)} ${event.total === 1 ? "problem" : "problems"}`;
  problemsTitle.textContent = event.total === 0 && event.done ? nothing : some;

  const examined =
    passKind === "dedup" ? "keys and elements checked" : "values checked";
  problemsState.textContent = event.done
    ? `${grouped(event.values)} ${examined}${capped}`
    : `checking… ${checked}%`;

  list.refresh();
}

validateButton.addEventListener("click", () => {
  passKind = "validate";
  resetProblems();
  problems.hidden = false;
  problemsTitle.textContent = "Checking…";
  problemsState.textContent = "";
  engine.call("validate", {}).catch((thrown: unknown) => {
    say("err", describe(thrown));
  });
});

dedupButton.addEventListener("click", (event) => {
  // Shift adds element comparison. A modifier rather than a second button
  // because it is the same question asked more thoroughly, and the toolbar has
  // to stay legible; the title attribute says so.
  const elements = event.shiftKey;
  passKind = "dedup";
  resetProblems();
  problems.hidden = false;
  problemsTitle.textContent = elements
    ? "Looking for repeated keys and records…"
    : "Looking for repeated keys…";
  problemsState.textContent = "";
  engine.call("dedup", { keys: true, elements }).catch((thrown: unknown) => {
    say("err", describe(thrown));
  });
});

pickSchema.addEventListener("click", () => {
  schemaFile.click();
});

schemaFile.addEventListener("change", () => {
  const chosen = schemaFile.files?.[0];
  if (!chosen) {
    return;
  }
  passKind = "schema";
  resetProblems();
  problems.hidden = false;
  problemsTitle.textContent = `Checking against ${chosen.name}…`;
  problemsState.textContent = "";

  // Read locally and hand over the text. A schema is small — kilobytes — which
  // is why it may be held whole where a document may not, and it is read here
  // rather than fetched because a remote `$ref` would need a host permission
  // the manifest deliberately does not request.
  chosen
    .text()
    .then((source) => engine.call("schema", { source }))
    .then(({ unsupported }) => {
      if (unsupported.length > 0) {
        say(
          "err",
          `${chosen.name}: ${unsupported.join(", ")} ${unsupported.length === 1 ? "is" : "are"} not checked by this validator.`,
        );
      }
    })
    .catch((thrown: unknown) => {
      problems.hidden = true;
      say("err", describe(thrown));
    });
});

problemsClose.addEventListener("click", () => {
  problems.hidden = true;
  void engine.call("validateStop", {}).catch(() => {
    // Nothing to stop is not a failure.
  });
});

// -------------------------------------------------------------- export

/** Whether an export is running, so a second click cannot start a second one. */
let exporting = false;

/**
 * Write the document to disk, a batch at a time.
 *
 * The loop is `convert → await write → convert`, and the `await` is the whole
 * design. Converting is fast and writing is not; without waiting for the write,
 * a 500 MB export would convert faster than the disk accepts and queue the
 * difference in memory — which is the failure the streaming write exists to
 * prevent, arrived at by a different route.
 *
 * With a filter active this writes the matching records only, because that is
 * what is on screen and what the user just asked a question about.
 */
async function runExport(): Promise<void> {
  if (exporting) {
    return;
  }
  const format = exportFormat.value as ExportFormat;
  const rows = filtered() ? [...search.matchedRows] : [];

  const picker = (
    window as unknown as {
      showSaveFilePicker?: (options: unknown) => Promise<FileSystemFileHandle>;
    }
  ).showSaveFilePicker;
  if (!picker) {
    say("err", "this browser cannot save files directly");
    return;
  }

  const extension =
    format === "csv" ? "csv" : format === "ndjson" ? "ndjson" : "json";
  const base = openName.replace(/\.(json|ndjson|jsonl)$/i, "");
  const suffix = rows.length > 0 ? "-filtered" : "";

  let handle: FileSystemFileHandle;
  try {
    handle = await picker({
      suggestedName: `${base}${suffix}.${extension}`,
      types: [
        {
          description: format.toUpperCase(),
          accept: { "text/plain": [`.${extension}`] },
        },
      ],
    });
  } catch {
    return; // The picker was dismissed. Not an error, and not worth saying.
  }

  exporting = true;
  exportButton.disabled = true;
  const writable = await handle.createWritable();

  try {
    let start: { format: ExportFormat; rows: number[] } | undefined = {
      format,
      rows,
    };
    let truncated = false;
    for (;;) {
      const step = await engine.call("exportStep", start ? { start } : {});
      start = undefined;
      truncated ||= step.truncated;

      if (step.chunk.byteLength > 0) {
        await writable.write(step.chunk);
      }
      if (step.done) {
        await writable.close();
        const what = rows.length > 0 ? "filtered records" : "records";
        say(
          "ok",
          `exported ${grouped(step.records)} ${what} to ${handle.name}`,
        );
        if (truncated) {
          // Never silent: an export that claims to be complete and is not is
          // the kind of thing that costs someone a day.
          say(
            "err",
            "some values were larger than the read limit and were cut short",
          );
        }
        break;
      }
      say("", `exporting… ${grouped(step.records)} records`);
    }
  } catch (thrown) {
    await writable.abort().catch(() => {
      // The stream is already gone; the original failure is the one to report.
    });
    void engine.call("exportStop", {}).catch(() => {});
    say("err", describe(thrown));
  } finally {
    exporting = false;
    exportButton.disabled = false;
  }
}

exportButton.addEventListener("click", () => {
  void runExport();
});

// ---------------------------------------------------------------- go to

/** How many children a key lookup will scan before giving up. */
const PATH_SCAN_LIMIT = 200_000;

/**
 * Find the child index matching `step` within `branch`.
 *
 * An index is O(1). A **key** is a scan: the engine indexes where children
 * start, not what they are called (C1), so the only way to find `"orders"` is
 * to materialize rows until one matches. Bounded by {@link PATH_SCAN_LIMIT} so
 * a wrong path against a five-million-element array stops rather than hangs.
 */
async function resolveStep(
  branch: Branch,
  step: Step,
): Promise<number | undefined> {
  if ("index" in step) {
    return step.index < branch.count ? step.index : undefined;
  }

  for (
    let index = 0;
    index < Math.min(branch.count, PATH_SCAN_LIMIT);
    index += BLOCK_ROWS
  ) {
    const { packed } = await engine.call("rows", {
      container: branch.container,
      start: index,
      count: BLOCK_ROWS,
    });
    const block = new RowBlock(packed);
    for (let n = 0; n < block.length; n++) {
      if (block.row(n).key === step.key) {
        return index + n;
      }
    }
    if (block.length === 0) {
      break;
    }
  }
  return undefined;
}

/**
 * Open the tree down to `steps` and select what it names.
 *
 * Each step opens the container it lands in, which is what makes the
 * destination reachable rather than merely known — the row has to exist in the
 * tree before the list can scroll to it.
 */
async function goToPath(steps: Step[]): Promise<boolean> {
  // A path addresses records, so a filtered view would be addressing something
  // else. Showing the whole file is the honest response to being given one.
  setFiltering(false);

  let branch = tree.root;
  for (const [depth, step] of steps.entries()) {
    const index = await resolveStep(branch, step);
    if (index === undefined) {
      return false;
    }

    const last = depth === steps.length - 1;
    if (last) {
      select({ branch, index });
      return true;
    }

    const row = await rowFor(branch, index);
    if (!row?.expandable) {
      return false;
    }
    const at: Located = { branch, index, depth };
    const existing = tree.branchOf(row.offset);
    if (existing) {
      branch = existing;
    } else {
      const extent = store.extentOf(row.offset);
      branch = tree.open(at, row.offset, extent.count, extent.complete);
      store.grow(row.offset, PREFETCH_ROWS);
      // The next step needs children to look at, so wait for the first batch.
      for (
        let spin = 0;
        spin < 200 && store.extentOf(row.offset).count === 0;
        spin++
      ) {
        await new Promise((resolve) => setTimeout(resolve, 10));
      }
      tree.setCount(
        branch,
        store.extentOf(row.offset).count,
        store.extentOf(row.offset).complete,
      );
    }
    sync();
  }
  return false;
}

/** Interpret whatever is in the box and go there. */
async function goTo(text: string): Promise<boolean> {
  const trimmed = text.trim();
  if (trimmed === "") {
    return false;
  }

  // `@` means a byte offset — the form an error message or a hex editor gives.
  if (trimmed.startsWith("@")) {
    const offset = Number(trimmed.slice(1).replace(/[_,\s]/g, ""));
    if (!Number.isFinite(offset) || offset < 0) {
      return false;
    }
    const { row } = await engine.call("locate", { offset });
    if (row === null) {
      return false;
    }
    setFiltering(false);
    select({ branch: tree.root, index: row });
    return true;
  }

  // A bare number is a row.
  if (/^\d[\d,_\s]*$/.test(trimmed)) {
    const row = Number(trimmed.replace(/[_,\s]/g, ""));
    if (!Number.isFinite(row) || row < 0 || row >= tree.root.count) {
      return false;
    }
    setFiltering(false);
    select({ branch: tree.root, index: row });
    return true;
  }

  const steps = parsePath(trimmed);
  return steps ? goToPath(steps) : false;
}

gotoInput.addEventListener("keydown", (event) => {
  if (event.key !== "Enter") {
    if (event.key === "Escape") {
      gotoInput.value = "";
      delete gotoInput.dataset["state"];
      viewport.focus();
    }
    return;
  }
  event.preventDefault();
  const text = gotoInput.value;
  void goTo(text)
    .then((found) => {
      gotoInput.dataset["state"] = found ? "" : "bad";
      say(
        found ? "ok" : "err",
        found ? `went to ${text.trim()}` : `no such row or path: ${text}`,
      );
      if (found) {
        viewport.focus();
      }
    })
    .catch((thrown: unknown) => {
      gotoInput.dataset["state"] = "bad";
      say("err", describe(thrown));
    });
});

collapseAllButton.addEventListener("click", () => {
  collapseAll();
  list.reset();
  sync();
  say("ok", "collapsed everything");
});

// ------------------------------------------------------------------- find

/** Which engine the current box contents reach. Drives the wording, not just the call. */
let searchMode: SearchMode = "literal";

/** A filter that would not compile, shown in place of a result count. */
let syntaxError = "";

/** Turn filtering on or off, and put the tree back in a consistent state. */
function setFiltering(on: boolean): void {
  if (filtering === on) {
    return;
  }
  filtering = on;
  findFilter.setAttribute("aria-pressed", on ? "true" : "false");
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
  syntaxError = "";
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
  searchMode = searchModeOf(needle);
  syntaxError = "";

  if (needle.length === 0) {
    resetSearch();
    void engine.call("findStop", {}).catch(() => {
      // Nothing to stop is the common case and not worth saying.
    });
    return;
  }

  // A filter's whole output is "these records matched", so the filtered view is
  // not an option alongside it — it *is* it. Turning it on here means the answer
  // is on screen rather than scattered through 1.7 million rows as highlights.
  if (searchMode === "filter") {
    setFiltering(true);
  }

  search.begin();
  renderFind();
  list.refresh();
  // Case-insensitive always, for now: it is what a find box is expected to do,
  // and a toggle is a control to explain in a toolbar that has to stay legible.
  // (A filter ignores it: `== "Error"` means what it says.)
  engine.call("find", { needle, caseSensitive: false, mode: searchMode }).then(
    ({ error }) => {
      if (error === null) {
        return;
      }
      // Not a toast: a half-typed expression is wrong for as long as it takes to
      // finish typing it, and three toasts a second is how a good error becomes
      // noise. It sits next to the box until the box is right.
      syntaxError = error;
      search.fail();
      renderFind();
    },
    (thrown: unknown) => {
      search.fail();
      say("err", describe(thrown));
      renderFind();
    },
  );
}

/**
 * Say something early if this file's *shape* will not fit.
 *
 * The index is 8 bytes per node whatever the node is, so cost is driven by how
 * many values a file has rather than how big it is: a record-shaped 8 GB NDJSON
 * needs 226 MB, and a 1 GB flat array of numbers needs 800 MB. The second shape
 * runs out of address space, and it does so *late* — after minutes of work that
 * is about to be thrown away.
 *
 * The projection uses only measured bytes: index-so-far divided by
 * bytes-consumed-so-far, multiplied out to the whole file. It waits for 2 % of
 * the file so the ratio has settled, and it is said once.
 */
function warnIfShapeIsExpensive(event: IndexProgress): void {
  if (warnedAboutShape || event.total === 0 || event.consumed === 0) {
    return;
  }
  const seen = event.consumed / event.total;
  if (seen < 0.02) {
    return;
  }

  const projected = (event.usage.index / event.consumed) * event.total;
  if (projected < INDEX_CEILING) {
    return;
  }

  warnedAboutShape = true;
  say(
    "err",
    `this file needs about ${humanBytes(projected)} of index — ` +
      `more than a browser can hold. Indexing will stop part-way, and ` +
      `everything found before then stays browsable.`,
  );
}

/** One instalment of results from the Worker. */
function onFound(event: FoundEvent): void {
  const first = search.size === 0;
  if (!search.accept(event)) {
    return; // A superseded search still posting. Not ours.
  }

  if (event.error) {
    say("err", describe(new EngineError(event.error)));
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
  findInput.dataset["scanning"] = search.scanning ? "true" : "false";
  findInput.dataset["mode"] = searchMode;
  findPrev.disabled = search.size === 0;
  findNext.disabled = search.size === 0;

  if (syntaxError !== "") {
    findStatus.textContent = syntaxError;
    findStatus.dataset["state"] = "error";
    return;
  }

  findStatus.textContent = describeSearch(search, findInput.value, searchMode);
  if (findInput.value.length > 0 && search.matches === 0 && !search.scanning) {
    findStatus.dataset["state"] = "none";
  } else {
    delete findStatus.dataset["state"];
  }
}

let findTimer: number | undefined;
findInput.addEventListener("input", () => {
  clearTimeout(findTimer);
  // Long enough that typing a word is one scan rather than five, short enough
  // that it feels like it reacted to the last keystroke.
  findTimer = self.setTimeout(runSearch, 200);
});

findInput.addEventListener("keydown", (event) => {
  if (event.key === "Enter") {
    event.preventDefault();
    clearTimeout(findTimer);
    if (search.size === 0) {
      runSearch();
    } else {
      goToMatch(search.at + (event.shiftKey ? -1 : 1));
    }
    return;
  }
  if (event.key === "Escape") {
    event.preventDefault();
    findInput.value = "";
    resetSearch();
    void engine.call("findStop", {}).catch(() => {
      // Stopping a scan that has already finished is not a failure.
    });
    viewport.focus();
  }
});

findFilter.addEventListener("click", () => {
  setFiltering(!filtering);
});

findNext.addEventListener("click", () => {
  goToMatch(search.at + 1);
});
findPrev.addEventListener("click", () => {
  goToMatch(search.at - 1);
});

// Ctrl/Cmd+F anywhere on the page, including from inside the tree — the whole
// point is that it is reachable without leaving the keyboard.
document.addEventListener("keydown", (event) => {
  if (findBar.hidden) {
    return;
  }
  if ((event.ctrlKey || event.metaKey) && event.key.toLowerCase() === "f") {
    event.preventDefault();
    findInput.focus();
    findInput.select();
    return;
  }
  if (event.altKey && event.key.toLowerCase() === "f") {
    event.preventDefault();
    setFiltering(!filtering);
    return;
  }
  if (event.altKey && event.key.toLowerCase() === "c") {
    event.preventDefault();
    collapseAll();
    list.reset();
    sync();
    return;
  }
  if ((event.ctrlKey || event.metaKey) && event.key.toLowerCase() === "g") {
    event.preventDefault();
    gotoInput.focus();
    gotoInput.select();
  }
});

// ------------------------------------------------------------------ files

/** Hand a file to the engine and show its root. */
/** The open file's name, for suggesting an export name. */
let openName = "leviathan";

async function openFile(source: File): Promise<void> {
  say("", `opening ${source.name}…`);
  openName = source.name;
  selection = undefined;
  copyPath.disabled = true;
  copyValue.disabled = true;
  tree.reset();
  store.clear();
  list.reset();
  list.setCount(0);
  findInput.value = "";
  resetSearch();
  resetProblems();
  warnedAboutShape = false;
  validating = 0;
  renderCrumbs();
  renderSelectionInfo();

  try {
    const { format, size } = await engine.call("open", { file: source });

    fileInfo.hidden = false;
    fileName.textContent = source.name;
    fileBytes = size;
    fileFacts.textContent = `${humanBytes(size)} · ${FORMAT_LABEL[format]}`;
    empty.hidden = true;
    viewport.hidden = false;
    indexing.hidden = false;
    findBar.hidden = false;
    gotoBar.hidden = false;
    viewport.focus();
    say("", "");

    if (format === "unknown" || format === "empty") {
      say("err", `Nothing to show — this file is ${FORMAT_LABEL[format]}.`);
    }
  } catch (thrown) {
    say("err", describe(thrown));
  }
}

function onEvent(event: WorkerEvent): void {
  if (event.kind === "ready") {
    setStatus("ready", "Engine ready");
    engineVersion.textContent = `engine ${event.core} · protocol ${event.protocol}`;
    return;
  }

  if (event.kind === "fatal") {
    setStatus("failed", "Engine failed to start");
    say("err", describe(new EngineError(event.error)));
    return;
  }

  if (event.kind === "found") {
    onFound(event);
    return;
  }

  if (event.kind === "validated") {
    onValidated(event);
    return;
  }

  const percent =
    event.total === 0 ? 100 : (event.consumed / event.total) * 100;
  progressFill.style.width = `${Math.min(100, percent).toFixed(1)}%`;
  busy = !event.done;
  cancel.disabled = event.done;
  viewport.setAttribute("aria-busy", busy ? "true" : "false");

  const rows = `${event.rows.toLocaleString()} rows`;
  if (!event.done) {
    progressFill.dataset["state"] = "";
    indexState.textContent = `${humanBytes(event.consumed)} · ${rows}`;
    warnIfShapeIsExpensive(event);
  } else if (event.stopped) {
    progressFill.dataset["state"] = "stopped";
    indexState.textContent = `${STOPPED[event.stopped]} · ${rows}`;
    if (event.error) {
      say("err", describe(new EngineError(event.error)));
    }
  } else {
    progressFill.dataset["state"] = "done";
    progressFill.style.width = "100%";
    indexState.textContent = rows;
  }

  // Tier 1 only ever appends, so a growing root is a taller scrollbar and
  // nothing else — no row on screen moves, and nothing cached is invalidated.
  renderUsage(event.usage);
  store.noteRoot(event.rows, event.done);
  rootRows = event.rows;
  rootComplete = event.done;
  sync();
}

// Confirms the loaded `.wasm` matches this bundle. Also the first message sent,
// which is what triggers instantiation in the Worker.
engine.checkVersion().catch((thrown: unknown) => {
  setStatus("failed", "Engine failed to start");
  say("err", describe(thrown));
});

cancel.addEventListener("click", () => {
  cancel.disabled = true;
  void engine.call("cancel", {}).catch(() => {
    // The final progress event reports the outcome; nothing to add here.
  });
});

for (const button of [pick, pickEmpty]) {
  button.addEventListener("click", () => {
    filePicker.click();
  });
}

pickFolder.addEventListener("click", () => {
  folderPicker.click();
});

/**
 * Offer the JSON files in a chosen folder.
 *
 * A folder picker hands over every file it contains, including the ones nobody
 * wants — `.DS_Store`, images, a 4 GB video. Filtering by extension keeps the
 * list to what this can actually open, and sorting by name keeps a directory of
 * dated exports in the order the names imply.
 *
 * One match opens immediately: making someone choose from a list of one is a
 * click that exists only because the code was easier to write that way.
 */
function offerFolder(files: File[]): void {
  const candidates = files
    .filter((file) => /\.(json|ndjson|jsonl)$/i.test(file.name))
    .sort((a, b) => a.name.localeCompare(b.name));

  fileList.replaceChildren();

  if (candidates.length === 0) {
    fileList.hidden = true;
    say(
      "err",
      `No .json, .ndjson or .jsonl files in that folder (${files.length} files seen).`,
    );
    return;
  }
  if (candidates.length === 1) {
    fileList.hidden = true;
    void openFile(candidates[0] as File);
    return;
  }

  for (const file of candidates) {
    const item = document.createElement("li");
    const button = document.createElement("button");
    button.type = "button";

    const name = document.createElement("span");
    name.textContent = file.name;
    const size = document.createElement("span");
    size.className = "size";
    size.textContent = humanBytes(file.size);

    button.append(name, size);
    button.addEventListener("click", () => {
      void openFile(file);
    });
    item.append(button);
    fileList.append(item);
  }

  fileList.hidden = false;
  say("ok", `${candidates.length} JSON files — choose one.`);
}

folderPicker.addEventListener("change", () => {
  offerFolder([...(folderPicker.files ?? [])]);
});

filePicker.addEventListener("change", () => {
  const chosen = filePicker.files?.[0];
  if (chosen) {
    void openFile(chosen);
  }
});

drop.addEventListener("click", (event) => {
  // The drop zone is itself a big click target, but it now contains real
  // controls — the folder button, and a list of files to choose from. A click
  // that landed on one of those has already been handled; opening the file
  // picker on top of it would replace the user's actual choice with a dialog.
  if (!(event.target as HTMLElement).closest("button")) {
    filePicker.click();
  }
});

drop.addEventListener("keydown", (event) => {
  if (event.key === "Enter" || event.key === " ") {
    event.preventDefault();
    filePicker.click();
  }
});

// Drops land anywhere on the page, not just on the empty state: once a file is
// open the drop zone is not on screen, and "drag another file in" is the most
// natural way to open the next one.
document.addEventListener("dragover", (event) => {
  event.preventDefault();
  drop.dataset["over"] = "true";
});

for (const type of ["dragleave", "drop"] as const) {
  document.addEventListener(type, () => {
    delete drop.dataset["over"];
  });
}

document.addEventListener("drop", (event) => {
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
paste.addEventListener("input", () => {
  clearTimeout(pasteTimer);
  pasteTimer = self.setTimeout(() => {
    const text = paste.value;
    if (text.length > 0) {
      void openFile(
        new File([text], "pasted.json", { type: "application/json" }),
      );
    }
  }, 150);
});
