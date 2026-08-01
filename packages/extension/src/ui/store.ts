/**
 * Rows, on demand, without ever holding many of them.
 *
 * The renderer paints synchronously — it is inside a scroll handler and it has
 * about sixteen milliseconds — while every row comes from a Worker across an
 * asynchronous boundary. This module is the join between those two facts:
 * {@link RowStore.rowAt} answers immediately with a row or with `undefined`,
 * and a row that was not there yet arrives later and triggers a repaint.
 *
 * Three things it is careful about, each learned from a way this goes wrong:
 *
 * - **Blocks, not rows.** A miss fetches an aligned block of
 *   {@link BLOCK_ROWS}, so scrolling costs one round-trip per block rather than
 *   one per row. This is C32 ("one read per screen") repeated at the message
 *   layer: the engine batches its file reads, and this batches its calls.
 * - **A short block is a question, not an answer.** The engine's expansion
 *   cache may evict any container at any moment (C36), and an evicted container
 *   reports zero rows — truthfully, since it has none *resident*. Treating that
 *   as "this node is empty" would paint a lie. The store re-expands and asks
 *   again, which is the price C36 deliberately chose to pay.
 * - **Bounded memory.** Cached blocks are capped and evicted least-recently-used,
 *   because a viewer that accumulates every row it has ever shown is a viewer
 *   that dies on the file it was built for — just more slowly.
 */

import type { NodeId } from '../protocol/index.js';
import { RowBlock, type Row } from '../protocol/rows.js';
import type { Engine } from './engine.js';

/**
 * Rows per fetch.
 *
 * Comfortably more than a screen (a 22 px row on a tall display is ~60), so an
 * ordinary scroll crosses a block boundary every second or so rather than every
 * frame, and small enough that a block is a few kilobytes rather than a stall.
 */
export const BLOCK_ROWS = 128;

/**
 * How many blocks stay resident.
 *
 * 256 blocks is ~32 000 rows — far more than anyone scrolls back through, and
 * a few megabytes at most, since a block holds only the short strings needed to
 * paint (previews are truncated by the engine, C33).
 */
const MAX_BLOCKS = 256;

/** What the store tells the page when the world changed underneath it. */
export interface StoreEvents {
  /** Rows arrived, or a fetch failed. The page should repaint. */
  rows(): void;
  /** A container's child count changed, from expansion or from indexing. */
  count(container: number, count: number, complete: boolean): void;
  /** A container stopped short of closing — truncated or malformed (C6). */
  incomplete(container: number): void;
  /** Something failed in a way worth telling the user about. */
  failed(thrown: unknown): void;
}

/** What is known about one container's children. */
interface Extent {
  count: number;
  /** Whether the engine has finished — no further child will appear. */
  complete: boolean;
}

/** A growth in progress, and the highest row anyone has asked it to reach. */
interface Growth {
  target: number;
}

export class RowStore {
  readonly #engine: Engine;
  readonly #events: StoreEvents;

  /** Decoded blocks, in least-recently-used order (a `Map` keeps insertion
   * order, and a re-insert on hit is the whole LRU). */
  readonly #blocks = new Map<string, RowBlock>();

  /** Blocks already requested, so a scroll does not ask ten times per frame. */
  readonly #inflight = new Set<string>();

  /** What each container is known to hold. */
  readonly #extents = new Map<number | 'root', Extent>();

  /** Expansions currently being driven, by container offset. */
  readonly #growing = new Map<number, Growth>();

  /** Bumped by {@link clear}, so answers to a closed file are discarded. */
  #generation = 0;

  constructor(engine: Engine, events: StoreEvents) {
    this.#engine = engine;
    this.#events = events;
  }

  /** Drop everything. What opening another file does. */
  clear(): void {
    this.#generation++;
    this.#blocks.clear();
    this.#inflight.clear();
    this.#extents.clear();
    this.#growing.clear();
  }

  /**
   * The row at `index` of `container`, if it is already here.
   *
   * Never blocks and never throws: a miss schedules the fetch and returns
   * `undefined`, which the renderer paints as a placeholder row. Rows are
   * addressed by container and index rather than by flat position because the
   * flat position of a row changes whenever anything above it opens, and a
   * cache keyed on something that moves is a cache that is always wrong.
   */
  rowAt(container: NodeId, index: number): Row | undefined {
    const start = index - (index % BLOCK_ROWS);
    const key = keyOf(container, start);
    const block = this.#blocks.get(key);

    if (!block) {
      this.#request(container, start);
      return undefined;
    }

    // Re-insert to move this block to the young end of the LRU.
    this.#blocks.delete(key);
    this.#blocks.set(key, block);

    const within = index - start;
    if (within >= block.length) {
      // Either the container grew since this block was fetched, or the engine
      // evicted it and answered short. Both are fixed by asking again.
      this.#blocks.delete(key);
      this.#request(container, start);
      return undefined;
    }
    return block.row(within);
  }

  /** What the store believes a container holds. */
  extentOf(container: NodeId): Extent {
    return this.#extents.get(container ?? 'root') ?? { count: 0, complete: false };
  }

  /** Record the root's extent, which comes from indexing progress, not expansion. */
  noteRoot(count: number, complete: boolean): void {
    const before = this.extentOf(null);
    this.#extents.set('root', { count, complete });
    if (count > before.count) {
      this.#invalidateFrom(null, before.count);
      this.#events.rows();
    }
  }

  /**
   * Index at least `upTo + 1` children of a container, in batches.
   *
   * Called when a node is opened and again whenever the viewport reaches the
   * end of what has been indexed, which is how a five-million-element array
   * fills in as it is scrolled rather than stalling when it is opened (C39).
   * Concurrent callers raise the target of the growth already running instead
   * of starting a second one — two expansions of the same container would do
   * the same work twice and interleave their progress reports.
   */
  grow(offset: number, upTo: number): void {
    const running = this.#growing.get(offset);
    if (running) {
      running.target = Math.max(running.target, upTo);
      return;
    }
    if (this.extentOf(offset).complete) {
      return;
    }

    const growth: Growth = { target: upTo };
    this.#growing.set(offset, growth);
    void this.#drive(offset, growth);
  }

  async #drive(offset: number, growth: Growth): Promise<void> {
    const generation = this.#generation;

    try {
      for (;;) {
        const step = await this.#engine.call('expand', { offset });
        if (generation !== this.#generation) {
          return; // Another file was opened while this was in flight.
        }

        const before = this.extentOf(offset);
        this.#extents.set(offset, { count: step.children, complete: step.done });
        if (step.children > before.count) {
          this.#invalidateFrom(offset, before.count);
        }
        this.#events.count(offset, step.children, step.done);
        this.#events.rows();

        if (step.done) {
          if (!step.complete) {
            this.#events.incomplete(offset);
          }
          return;
        }
        if (step.children > growth.target) {
          return;
        }
      }
    } catch (thrown) {
      if (generation === this.#generation) {
        this.#events.failed(thrown);
      }
    } finally {
      if (this.#growing.get(offset) === growth) {
        this.#growing.delete(offset);
      }
    }
  }

  /** Tell the engine a container will not be looked at again. */
  forget(offset: number): void {
    this.#extents.delete(offset);
    this.#growing.delete(offset);
    for (const key of [...this.#blocks.keys()]) {
      if (key.startsWith(`${offset}:`)) {
        this.#blocks.delete(key);
      }
    }
    void this.#engine.call('forget', { offset }).catch(() => {
      // Purely advisory: the cache evicts on its own and a byte offset stays
      // valid whether or not its expansion is resident (C36).
    });
  }

  async #request(container: NodeId, start: number): Promise<void> {
    const key = keyOf(container, start);
    if (this.#inflight.has(key)) {
      return;
    }
    this.#inflight.add(key);
    const generation = this.#generation;

    try {
      let block = await this.#fetch(container, start);

      // A short block from a container that is known to hold more rows means
      // the engine's cache dropped the expansion. Rebuild it and ask once more
      // — the rebuilt table is byte-identical, so nothing the UI holds becomes
      // stale (C36).
      const extent = this.extentOf(container);
      if (
        typeof container === 'number' &&
        start + block.length < Math.min(extent.count, start + BLOCK_ROWS)
      ) {
        await this.#rebuild(container, start + BLOCK_ROWS - 1);
        if (generation !== this.#generation) {
          return;
        }
        block = await this.#fetch(container, start);
      }

      if (generation !== this.#generation) {
        return; // The file this belonged to is closed.
      }
      this.#blocks.set(key, block);
      this.#evict();
      this.#events.rows();
    } catch (thrown) {
      if (generation === this.#generation) {
        this.#events.failed(thrown);
        this.#events.rows();
      }
    } finally {
      this.#inflight.delete(key);
    }
  }

  async #fetch(container: NodeId, start: number): Promise<RowBlock> {
    const { packed } = await this.#engine.call('rows', {
      container,
      start,
      count: BLOCK_ROWS,
    });
    return new RowBlock(packed);
  }

  /** Re-run an evicted container's expansion until it covers `upTo`. */
  async #rebuild(offset: number, upTo: number): Promise<void> {
    const generation = this.#generation;
    for (;;) {
      const step = await this.#engine.call('expand', { offset });
      if (generation !== this.#generation) {
        return;
      }
      this.#extents.set(offset, { count: step.children, complete: step.done });
      if (step.done || step.children > upTo) {
        return;
      }
    }
  }

  /** Drop cached blocks that may be short because a container grew. */
  #invalidateFrom(container: NodeId, index: number): void {
    const prefix = `${container ?? 'r'}:`;
    const from = index - (index % BLOCK_ROWS);
    for (const key of [...this.#blocks.keys()]) {
      if (!key.startsWith(prefix)) {
        continue;
      }
      const start = Number(key.slice(prefix.length));
      if (start >= from) {
        this.#blocks.delete(key);
      }
    }
  }

  #evict(): void {
    while (this.#blocks.size > MAX_BLOCKS) {
      const oldest = this.#blocks.keys().next();
      if (oldest.done) {
        return;
      }
      this.#blocks.delete(oldest.value);
    }
  }
}

function keyOf(container: NodeId, start: number): string {
  return `${container ?? 'r'}:${start}`;
}
