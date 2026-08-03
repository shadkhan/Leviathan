/**
 * What the find bar knows, with none of what it looks like.
 *
 * Search results arrive in instalments from a Worker that may be running a scan
 * the user has already replaced, and the user is stepping through them with a
 * key that wraps at both ends. That is a small state machine, and small state
 * machines are where off-by-ones live — so it is here, DOM-free and engine-free,
 * tested directly (`scripts/ui.test.mjs`), for the same reason {@link Tree} is
 * (C45: the layer with no test is the layer with the bug).
 *
 * Two rules it exists to hold:
 *
 * - **Late results from a dead search are discarded, not cancelled.** Every
 *   keystroke starts a new scan and the previous one keeps posting for a frame
 *   or two. Ids are assigned by the Worker and only ever increase, so "is this
 *   still mine?" is a comparison rather than a protocol.
 * - **A row may appear twice.** Two hits inside one record are two results,
 *   because pressing Enter twelve times has to agree with a count of twelve.
 *   {@link Search.marked} deduplicates for painting; {@link Search.size} does
 *   not, for counting.
 */

import type { FoundEvent } from '../protocol/index.js';

export class Search {
  /** Root row indices, ascending, one per match. Duplicates are meaningful. */
  #rows: number[] = [];

  /** Distinct rows holding at least one match, for painting. */
  #marked = new Set<number>();

  /**
   * The same rows, distinct and ascending — the filtered view's contents.
   *
   * Maintained as results arrive rather than sorted on demand: matches are
   * found in file order, so a new row is either the one already at the end or
   * greater than it, and appending is the whole algorithm. Sorting a growing
   * array on every instalment would be the one part of filtering that scales
   * with the number of matches rather than with the screen.
   */
  #distinct: number[] = [];

  /** Index into {@link #rows} of the match the user is standing on. */
  #at = -1;

  /** Highest search id seen. Assigned by the Worker, never by this. */
  #id = 0;

  /** Results from a search at or below this id are not ours. */
  #stale = 0;

  #matches = 0;
  #pending = 0;
  #limited = false;
  #scanning = false;

  /** Results so far, counting a row once per hit in it. */
  get size(): number {
    return this.#rows.length;
  }

  /** Position within the results, or `-1` before the first jump. */
  get at(): number {
    return this.#at;
  }

  /** The row the current result is in, if there is one. */
  get row(): number | undefined {
    return this.#at < 0 ? undefined : this.#rows[this.#at];
  }

  /** Total matches reported, including any with no row to visit. */
  get matches(): number {
    return this.#matches;
  }

  /** Matches beyond the indexed region — real, and unreachable. */
  get pending(): number {
    return this.#pending;
  }

  /** Whether the count is a floor rather than a total. */
  get limited(): boolean {
    return this.#limited;
  }

  /** Whether a scan is still running. */
  get scanning(): boolean {
    return this.#scanning;
  }

  /** Rows holding at least one match, ascending — what a filtered tree shows. */
  get matchedRows(): readonly number[] {
    return this.#distinct;
  }

  /**
   * Where `row` sits in the filtered view, or `-1`.
   *
   * A binary search because this runs once per painted row: with ten thousand
   * matches a linear scan would be five thousand comparisons per row, sixty
   * times a second.
   */
  positionOf(row: number): number {
    let low = 0;
    let high = this.#distinct.length - 1;
    while (low <= high) {
      const mid = (low + high) >> 1;
      const at = this.#distinct[mid] as number;
      if (at === row) return mid;
      if (at < row) low = mid + 1;
      else high = mid - 1;
    }
    return -1;
  }

  /** How a row should be painted, if at all. */
  mark(row: number): 'current' | 'match' | undefined {
    if (!this.#marked.has(row)) {
      return undefined;
    }
    return this.row === row ? 'current' : 'match';
  }

  /**
   * Discard everything and disown every search so far.
   *
   * What clearing the box, opening another file, or starting a new scan does.
   * Disowning by id rather than by a flag is what makes a stale instalment
   * arriving one frame later a no-op instead of a resurrection.
   */
  reset(): void {
    this.#stale = this.#id;
    this.#rows = [];
    this.#marked = new Set();
    this.#distinct = [];
    this.#at = -1;
    this.#matches = 0;
    this.#pending = 0;
    this.#limited = false;
    this.#scanning = false;
  }

  /** Note that a scan has been asked for, before any results exist. */
  begin(): void {
    this.reset();
    this.#scanning = true;
  }

  /** Note that a scan could not be started. */
  fail(): void {
    this.#scanning = false;
  }

  /**
   * Take one instalment of results.
   *
   * Returns whether it was ours — a caller repaints only if it was.
   */
  accept(event: FoundEvent): boolean {
    if (event.search <= this.#stale) {
      return false;
    }
    if (event.search > this.#id) {
      this.#id = event.search;
    }

    for (const row of event.rows) {
      this.#rows.push(row);
      if (!this.#marked.has(row)) {
        this.#marked.add(row);
        this.#distinct.push(row);
      }
    }
    this.#matches = event.matches;
    this.#pending = event.pending;
    this.#limited = event.limited;
    this.#scanning = !event.done;
    return true;
  }

  /**
   * Move to the `n`-th result, wrapping at both ends, and return its row.
   *
   * Wrapping rather than clamping because that is what every find box does:
   * pressing Enter past the last match goes back to the first, and the
   * alternative is a key that silently stops working.
   */
  goTo(n: number): number | undefined {
    if (this.#rows.length === 0) {
      return undefined;
    }
    const count = this.#rows.length;
    this.#at = ((n % count) + count) % count;
    return this.#rows[this.#at];
  }
}

/**
 * The line shown next to the find box.
 *
 * Here rather than in the renderer because every clause of it is a claim about
 * numbers that must not be overstated: `limited` makes the count a floor and it
 * is printed with a `+`; `pending` is matches with no row, and saying nothing
 * about them would let "1,024 matches" sit above a list of 890.
 */
export function describeSearch(search: Search, needle: string): string {
  if (needle.length === 0) {
    return '';
  }
  if (search.matches === 0) {
    return search.scanning ? 'searching…' : 'no matches';
  }

  const total = `${search.matches.toLocaleString()}${search.limited ? '+' : ''}`;
  const position = search.at >= 0 ? `${(search.at + 1).toLocaleString()} of ` : '';
  const scanning = search.scanning ? '…' : '';
  const unreachable = search.pending > 0 ? ` · ${search.pending.toLocaleString()} unindexed` : '';
  return `${position}${total}${scanning}${unreachable}`;
}
