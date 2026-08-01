/**
 * The shape of the tree as the user has opened it.
 *
 * A virtual list asks one question, sixty times a second: *what is row 4 812
 * 907?* Everything in this file exists to answer that in microseconds without
 * anyone ever having built a list of rows.
 *
 * ## Why there is no array of rows
 *
 * The obvious model is a flat array of visible nodes, spliced on expand and
 * collapse. Expanding a five-million-element array would allocate five million
 * entries on the main thread — the exact failure the engine was built to avoid,
 * reintroduced one layer above it. So nothing here is proportional to the number
 * of *rows*; everything is proportional to the number of *expansions*, which is
 * a few dozen because a person opens a few dozen nodes by hand.
 *
 * A {@link Branch} is one opened container: how many children it currently has,
 * and which of those children are themselves open. `size` is the number of rows
 * its subtree contributes, kept up to date on every change, so the total row
 * count is one field read and locating a row is a walk over open branches only.
 *
 * ## Ids
 *
 * A container is addressed by its byte offset, exactly as the engine addresses
 * it (`DEEP_REASONING.md` C36). Nothing here holds an index into anything the
 * engine owns, so an eviction in the engine's expansion cache cannot invalidate
 * a single field in this file — it costs work, never addressability.
 *
 * This module is deliberately DOM-free and engine-free: it is arithmetic over a
 * small structure, and it is tested as such (`scripts/ui.test.mjs`).
 */

import type { NodeId } from '../protocol/index.js';

/** One container the user has opened, and the state of its subtree. */
export interface Branch {
  /** Byte offset of the container, or `null` for the document root. */
  readonly container: NodeId;
  /** Children the engine has indexed so far — the rows this branch offers. */
  count: number;
  /** Whether the engine has finished indexing this container's children. */
  complete: boolean;
  /** Rows this subtree contributes: `count` plus every open descendant. */
  size: number;
  /** The branch this one hangs from, or `null` for the root. */
  readonly parent: Branch | null;
  /** Which child of {@link parent} this branch is. `-1` for the root. */
  readonly indexInParent: number;
  /** Open children, kept sorted by `indexInParent` so a walk is in row order. */
  readonly children: Branch[];
}

/** Where a flat row index lands. */
export interface Located {
  /** The container the row belongs to. */
  readonly branch: Branch;
  /** The row's index within that container. */
  readonly index: number;
  /** How deep the row sits, for indentation. The root's own rows are depth 0. */
  readonly depth: number;
}

export class Tree {
  #root: Branch = rootBranch();

  /** Every open container, by byte offset — so a row can ask "am I open?". */
  readonly #open = new Map<number, Branch>();

  /** Total rows currently visible. The virtual list's scroll height. */
  get size(): number {
    return this.#root.size;
  }

  /** The document root, whose rows are tier 1. */
  get root(): Branch {
    return this.#root;
  }

  /** Forget everything. What opening a new file does. */
  reset(): void {
    this.#root = rootBranch();
    this.#open.clear();
  }

  /**
   * Update how many children a container currently offers.
   *
   * Counts only ever grow — the root's while tier 1 indexes, a container's
   * while tier 2 expands — so this is how a partially indexed file becomes a
   * taller scrollbar without anything being rebuilt.
   */
  setCount(branch: Branch, count: number, complete: boolean): void {
    if (count === branch.count && complete === branch.complete) {
      return;
    }
    branch.count = count;
    branch.complete = complete;
    resize(branch);
  }

  /** The open branch for a container's byte offset, if it is open. */
  branchOf(offset: number): Branch | undefined {
    return this.#open.get(offset);
  }

  /**
   * Open the container at `at`, which must be a row that has a container value.
   *
   * `count` is what the engine has indexed of it so far; the caller keeps
   * feeding {@link setCount} as expansion proceeds.
   */
  open(at: Located, offset: number, count: number, complete: boolean): Branch {
    const existing = this.#open.get(offset);
    if (existing) {
      return existing;
    }

    const branch: Branch = {
      container: offset,
      count,
      complete,
      size: count,
      parent: at.branch,
      indexInParent: at.index,
      children: [],
    };

    // Sorted insert. The list is short (open siblings, not siblings), and
    // keeping it ordered is what lets `locate` walk it once, in row order.
    const siblings = at.branch.children;
    let position = siblings.length;
    for (let i = 0; i < siblings.length; i++) {
      if ((siblings[i] as Branch).indexInParent > at.index) {
        position = i;
        break;
      }
    }
    siblings.splice(position, 0, branch);

    this.#open.set(offset, branch);
    resize(at.branch);
    return branch;
  }

  /**
   * Close a branch and return every container that is no longer open.
   *
   * Descendants are closed with it, and they are returned rather than acted on:
   * telling the engine to forget them is the caller's business, and it is
   * optional in any case — the engine's cache evicts on its own and a byte
   * offset stays valid either way (C36).
   */
  close(branch: Branch): number[] {
    const parent = branch.parent;
    if (!parent) {
      return []; // The root is not closable; there would be nothing to show.
    }

    const at = parent.children.indexOf(branch);
    if (at >= 0) {
      parent.children.splice(at, 1);
    }

    const forgotten: number[] = [];
    const stack = [branch];
    while (stack.length > 0) {
      const node = stack.pop() as Branch;
      if (typeof node.container === 'number') {
        this.#open.delete(node.container);
        forgotten.push(node.container);
      }
      stack.push(...node.children);
    }

    resize(parent);
    return forgotten;
  }

  /**
   * Which row a flat index is.
   *
   * The walk descends only through *open* branches, so its cost is the depth of
   * the tree times the number of open siblings at each level — tens of
   * comparisons, not millions. Out-of-range indices clamp to the last row
   * rather than throwing: a scroll handler racing a shrinking list is normal,
   * and a thrown error mid-frame is not a useful answer to it. Callers must not
   * ask at all when {@link size} is zero — there is no row to name.
   */
  locate(flat: number): Located {
    let branch = this.#root;
    let depth = 0;
    let want = Math.max(0, Math.min(Math.floor(flat), this.#root.size - 1));

    for (;;) {
      // `cursor` is the next child index of `branch` not yet accounted for;
      // `seen` is how many rows of this subtree the walk has passed.
      let cursor = 0;
      let seen = 0;
      let descended = false;

      for (const child of branch.children) {
        const plain = child.indexInParent - cursor;
        if (want < seen + plain) {
          return { branch, index: cursor + (want - seen), depth };
        }
        seen += plain;

        // The open child's own row, which belongs to `branch`.
        if (want === seen) {
          return { branch, index: child.indexInParent, depth };
        }
        seen += 1;

        if (want < seen + child.size) {
          want -= seen;
          branch = child;
          depth += 1;
          descended = true;
          break;
        }

        seen += child.size;
        cursor = child.indexInParent + 1;
      }

      if (!descended) {
        return { branch, index: cursor + (want - seen), depth };
      }
    }
  }

  /**
   * The flat index of child `index` of `branch` — the inverse of
   * {@link locate}, for scrolling to a row that is not on screen.
   */
  flatIndexOf(branch: Branch, index: number): number {
    let local = index;
    for (const child of branch.children) {
      if (child.indexInParent >= index) {
        break;
      }
      local += child.size;
    }

    const parent = branch.parent;
    if (!parent) {
      return local;
    }
    // The branch's own row sits one above the first of its children.
    return this.flatIndexOf(parent, branch.indexInParent) + 1 + local;
  }
}

function rootBranch(): Branch {
  return {
    container: null,
    count: 0,
    complete: false,
    size: 0,
    parent: null,
    indexInParent: -1,
    children: [],
  };
}

/**
 * Recompute `size` for a branch and its ancestors.
 *
 * Walking up rather than down is the whole reason opening a node is O(depth)
 * instead of O(rows): the sizes below are already correct, and only the chain
 * to the root learned something.
 */
function resize(from: Branch): void {
  let branch: Branch | null = from;
  while (branch) {
    let size = branch.count;
    for (const child of branch.children) {
      size += child.size;
    }
    if (size === branch.size) {
      return; // Unchanged here means unchanged in every sum above.
    }
    branch.size = size;
    branch = branch.parent;
  }
}
