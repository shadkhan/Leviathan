/**
 * The decoder for the packed row buffer — counterpart to `leviathan-wasm/src/pack.rs`.
 *
 * The two files describe one byte layout from opposite sides, and nothing but
 * {@link ROW_LAYOUT_VERSION} enforces that they agree. Read the Rust module's
 * docs for the layout and the reasoning; this file assumes both.
 *
 * ## Why this is not `JSON.parse` of a message
 *
 * A screen of rows is fifty objects with two strings each. Sent the ordinary
 * way that is a hundred and fifty allocations per scroll tick, on the thread
 * that is trying to paint. Here it is one `ArrayBuffer`, transferred rather
 * than copied, read through a `DataView` — and a row's strings are decoded
 * only when {@link decodeRow} is called for it, so a buffer that is scrolled
 * past costs nothing beyond the transfer.
 */

/** Must equal `rowLayoutVersion()` from the WASM module. */
export const ROW_LAYOUT_VERSION = 1;

/** Bytes before the first row record. */
const HEADER_BYTES = 16;

/** Bytes per row record. */
const ROW_BYTES = 40;

/**
 * `pack.rs` writes `u64::MAX` for a value whose extent was never determined.
 *
 * Tested against `MAX_SAFE_INTEGER` rather than for equality with 2^64−1: that
 * constant is not representable as a double, so an equality check would be
 * comparing two different roundings and hoping. Every real offset is below this
 * bound by the same argument that lets offsets be doubles at all.
 */
const UNKNOWN_END = Number.MAX_SAFE_INTEGER;

/** What kind of value a row holds. Mirrors `leviathan_core::ValueKind`. */
export type RowKind =
  | "object"
  | "array"
  | "string"
  | "number"
  | "true"
  | "false"
  | "null"
  | "invalid";

/** Indexed by the discriminant written in `pack.rs`. Order is the contract. */
const KINDS: readonly RowKind[] = [
  "object",
  "array",
  "string",
  "number",
  "true",
  "false",
  "null",
  "invalid",
];

const FLAG_COUNT_EXACT = 1 << 0;
const FLAG_TRUNCATED = 1 << 1;
const FLAG_HAS_KEY = 1 << 2;
const FLAG_EXPANDABLE = 1 << 3;

/** One row of the tree, ready to paint. Mirrors `leviathan_core::Row`. */
export interface Row {
  /** Byte offset the row starts at — its stable identity, and what `expand`
   * and `forget` are addressed by. */
  offset: number;
  /** Byte offset of the value itself, which differs from `offset` when there
   * is a key. */
  valueStart: number;
  /** One past the value, or `null` when it ran past its budget. */
  valueEnd: number | null;
  kind: RowKind;
  /** The member's key. `null` for an array element — which is not the same as
   * `''`, a legal object key. */
  key: string | null;
  /** A short rendering of the value. Empty for containers. */
  preview: string;
  /** Whether `preview` was cut short. */
  truncated: boolean;
  /** Children, for containers. */
  children: number;
  /** Whether `children` is exact rather than a lower bound (C33). A `false`
   * here is what the UI renders as `1,000+`. */
  childrenExact: boolean;
  /** Whether the row can be expanded. */
  expandable: boolean;
}

/** A decoded buffer of rows, read lazily. */
export class RowBlock {
  readonly #bytes: Uint8Array;
  readonly #view: DataView;
  readonly #count: number;
  /** Where each row's key begins in the string blob. Built once, on demand,
   * because strings are stored as lengths and finding row *n* otherwise means
   * walking rows 0..n. */
  #stringStarts: Uint32Array | undefined;

  /**
   * Rows already decoded, kept because a scrolling list paints the same row on
   * consecutive frames.
   *
   * Decoding allocates an object and two strings. At sixty frames a second over
   * fifty visible rows that is thousands of short-lived allocations per second
   * — a garbage-collection profile rather than a slow-function one, which is
   * exactly the shape of the frame-time tail measured at M2 (C53). A row is
   * immutable once decoded, so this is a cache with no invalidation.
   */
  #decoded: (Row | undefined)[] | undefined;

  constructor(buffer: ArrayBuffer) {
    this.#bytes = new Uint8Array(buffer);
    this.#view = new DataView(buffer);

    if (buffer.byteLength < HEADER_BYTES) {
      throw new Error("Row buffer is too short to contain a header.");
    }

    const version = this.#view.getUint32(0, true);
    if (version !== ROW_LAYOUT_VERSION) {
      throw new Error(
        `Row layout mismatch: the UI reads v${ROW_LAYOUT_VERSION}, the engine wrote v${version}. Rebuild the extension with \`pnpm build\`.`,
      );
    }

    this.#count = this.#view.getUint32(4, true);
    const strings = this.#view.getUint32(8, true);
    const expected = HEADER_BYTES + this.#count * ROW_BYTES + strings;
    if (buffer.byteLength !== expected) {
      throw new Error(
        `Row buffer is ${buffer.byteLength} bytes; its header describes ${expected}.`,
      );
    }
  }

  /** How many rows the buffer holds. */
  get length(): number {
    return this.#count;
  }

  /** Decode row `index`, including its strings. Cached after the first call. */
  row(index: number): Row {
    if (index < 0 || index >= this.#count) {
      throw new RangeError(
        `Row ${index} is outside a block of ${this.#count}.`,
      );
    }

    const cache = (this.#decoded ??= new Array<Row | undefined>(this.#count));
    const hit = cache[index];
    if (hit) {
      return hit;
    }

    const decoded = this.#decode(index);
    cache[index] = decoded;
    return decoded;
  }

  #decode(index: number): Row {
    const at = HEADER_BYTES + index * ROW_BYTES;
    const view = this.#view;
    const flags = view.getUint8(at + 33);
    const valueEnd = u64(view, at + 16);

    const keyLength = view.getUint16(at + 34, true);
    const previewLength = view.getUint32(at + 36, true);
    const keyAt = this.#stringStart(index);
    const previewAt = keyAt + keyLength;

    return {
      offset: u64(view, at),
      valueStart: u64(view, at + 8),
      valueEnd: valueEnd >= UNKNOWN_END ? null : valueEnd,
      kind: KINDS[view.getUint8(at + 32)] ?? "invalid",
      key: (flags & FLAG_HAS_KEY) === 0 ? null : this.#text(keyAt, keyLength),
      preview: this.#text(previewAt, previewLength),
      truncated: (flags & FLAG_TRUNCATED) !== 0,
      children: u64(view, at + 24),
      childrenExact: (flags & FLAG_COUNT_EXACT) !== 0,
      expandable: (flags & FLAG_EXPANDABLE) !== 0,
    };
  }

  /** Every row, decoded. Convenient for tests and small blocks. */
  all(): Row[] {
    const rows: Row[] = new Array<Row>(this.#count);
    for (let i = 0; i < this.#count; i++) {
      rows[i] = this.row(i);
    }
    return rows;
  }

  /**
   * Byte offset of row `index`'s key within the string blob.
   *
   * The prefix sum is built on first use and kept, so random access to a row is
   * O(1) after the first — which matters because a virtual list paints rows in
   * whatever order the scroll position dictates, not in order.
   */
  #stringStart(index: number): number {
    let starts = this.#stringStarts;
    if (!starts) {
      starts = new Uint32Array(this.#count + 1);
      starts[0] = HEADER_BYTES + this.#count * ROW_BYTES;
      for (let i = 0; i < this.#count; i++) {
        const at = HEADER_BYTES + i * ROW_BYTES;
        starts[i + 1] =
          (starts[i] as number) +
          this.#view.getUint16(at + 34, true) +
          this.#view.getUint32(at + 36, true);
      }
      this.#stringStarts = starts;
    }
    return starts[index] as number;
  }

  #text(at: number, length: number): string {
    return length === 0
      ? ""
      : DECODER.decode(this.#bytes.subarray(at, at + length));
  }
}

const DECODER = new TextDecoder();

/**
 * Read a `u64` as a JavaScript number.
 *
 * Two `u32` reads rather than `getBigUint64`, for the same reason offsets cross
 * the WASM boundary as doubles: every value below 2^53 is exact, no file will
 * ever exceed it, and `BigInt` in a renderer's hot path is a cost with nothing
 * to buy.
 */
function u64(view: DataView, at: number): number {
  return (
    view.getUint32(at, true) + view.getUint32(at + 4, true) * 0x1_0000_0000
  );
}
