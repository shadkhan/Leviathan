/**
 * The typed message contract between the UI thread and the Worker.
 *
 * This module is imported by *both* sides and must stay free of DOM and Worker
 * globals — it is types plus two tiny helpers, nothing else. Keeping it neutral
 * is what lets `tsc` prove that a request the UI sends is a request the Worker
 * knows how to answer.
 *
 * ## Shape
 *
 * Two directions, deliberately different in kind:
 *
 * - **Calls** (UI → Worker → UI) are request/response, correlated by a numeric
 *   id. Every call is declared once in {@link Calls}; the envelopes and the
 *   client's method signatures are all derived from that one declaration, so
 *   adding a call in M1 means adding a line there and nothing else.
 * - **Events** (Worker → UI, unsolicited) carry things that are not answers:
 *   startup readiness, indexing progress, a fatal init failure.
 *
 * ## Why hand-rolled and not Comlink
 *
 * Bundle budget (SPEC §M2: ≤150 KB gz for all JS/CSS). Comlink is small but its
 * proxy model hides *when* a round-trip happens, and the whole performance story
 * here depends on round-trips being counted deliberately — one per animation
 * frame, batched. See `docs/adr/ADR-002`.
 */

/**
 * Bumped whenever an existing call's shape changes incompatibly.
 *
 * The UI asserts the Worker reports the same number at startup. A stale
 * `dist/` after a rebuild is the single most likely cause of a confusing bug in
 * this project, so it fails loudly at boot instead of subtly at use.
 */
export const PROTOCOL_VERSION = 5;

/**
 * What the engine currently occupies, for the memory readout.
 *
 * Requirement 9 is that bounded memory be *visible*: a user handing a viewer a
 * gigabyte wants to watch it stay bounded, not be told that it does. Every
 * number here is the engine's own — the index it built and the linear memory it
 * asked the browser for — rather than a guess from `performance.memory`, which
 * measures the JS heap of the wrong thread and is not in any standard.
 */
export interface Usage {
  /** Bytes of index held: tier 1 plus every resident expansion. */
  index: number;
  /** WASM linear memory, which only ever grows. The engine's true footprint. */
  heap: number;
}

/** Input shapes the core can index. Mirrors `leviathan_core::Format`. */
export type Format = 'single-document' | 'ndjson' | 'empty' | 'unknown';

/** A failure, flattened for structured-clone transport across the boundary. */
export interface ProtocolError {
  /** Human-readable, already suitable for display. */
  message: string;
  /** Underlying detail (a Rust panic message, a DOM exception name). */
  cause?: string;
}

/**
 * A container's byte offset, or `null` for the document root.
 *
 * Byte offsets are the node ids of this system, and they are ordinary numbers
 * rather than `BigInt`s — see `rows.ts` and `DEEP_REASONING.md` C36 and C43.
 */
export type NodeId = number | null;

/**
 * Every call the UI can make, as `method → { params, result }`.
 *
 * This is the single source of truth for the request surface.
 */
export interface Calls {
  /**
   * Round-trip a `u32` through Worker → WASM → back.
   *
   * The M0 exit criterion, kept permanently as a startup self-check: if the
   * WASM module is missing, stale, or failed to instantiate, this is what says
   * so.
   */
  echo: { params: { value: number }; result: { value: number } };

  /** Report the engine and protocol versions the Worker actually loaded. */
  version: {
    params: Record<string, never>;
    result: { core: string; protocol: number };
  };

  /**
   * Detect single-document vs NDJSON from a prefix of the input.
   *
   * `prefix` is transferred, not copied — see {@link transferables}. Callers
   * send a bounded prefix ({@link SNIFF_PREFIX_BYTES}), never a whole file:
   * bytes that cross this boundary are copied into WASM memory, and the entire
   * design rests on that never happening at file scale.
   */
  sniff: { params: { prefix: ArrayBuffer }; result: { format: Format } };

  /**
   * Take ownership of a file and begin indexing it.
   *
   * The `File` is structure-cloned, which moves a *handle*, not the bytes — the
   * Worker reads ranges out of it on demand and the file is never held whole
   * anywhere. This resolves as soon as the format is known, which is one 64 KiB
   * read: the UI can name the file and its format immediately, seconds before
   * the tree is finished. Indexing then continues on its own and reports
   * through {@link WorkerEvent}s of kind `progress`.
   *
   * Opening a second file replaces the first.
   */
  open: {
    params: { file: File };
    result: { format: Format; size: number };
  };

  /**
   * Stop indexing where it is.
   *
   * The partial index stays usable — every row found so far is a real row — so
   * this is "that is enough of this file", not "throw it away".
   */
  cancel: { params: Record<string, never>; result: { consumed: number; rows: number } };

  /** How many rows a container currently offers. `null` is the root. */
  rowCount: { params: { container: NodeId }; result: { count: number } };

  /**
   * Materialize a run of rows, packed into one transferable buffer.
   *
   * Decode with `RowBlock` from `./rows.js`. The buffer is transferred, so the
   * Worker's copy is detached the moment it is sent — which is the point, and
   * why nothing here is ever shared by reference.
   */
  rows: {
    params: { container: NodeId; start: number; count: number };
    result: { packed: ArrayBuffer };
  };

  /**
   * Index the next batch of one container's children, and say how far it got.
   *
   * Called repeatedly by the UI for a container large enough to need it. The
   * children found so far are addressable after every call, so a five-million
   * element array fills in rather than blocking (`DEEP_REASONING.md` C39).
   */
  expand: {
    params: { offset: number };
    result: { children: number; done: boolean; complete: boolean; usage: Usage };
  };

  /**
   * Read a value's bytes back out of the file, as text.
   *
   * What "copy value" needs and what a row cannot give it: a row's preview is
   * truncated by design (`DEEP_REASONING.md` C33), so copying what is on screen
   * would silently hand over the wrong thing. `end` is `null` for a value whose
   * extent was never determined — the read then runs to `limit` and says it was
   * cut short, because a single value can be larger than memory and the
   * clipboard is not where that should be discovered.
   */
  text: {
    params: { start: number; end: number | null; limit: number };
    result: { text: string; truncated: boolean };
  };

  /**
   * Forget a container's expansion — what collapsing a node does.
   *
   * Never required for correctness: the cache evicts on its own, and a byte
   * offset stays valid whether or not its expansion is resident (C36). This is
   * the UI telling the engine what it already knows it will not need.
   */
  forget: { params: { offset: number }; result: Record<string, never> };

  /**
   * Search the file's bytes for a literal string.
   *
   * Starts a scan and returns immediately; results arrive as `found` events
   * (see {@link WorkerEvent}) until one reports `done`. A scan replaces any
   * scan already running — typing another character is a new search, and the
   * old one's remaining work is worthless.
   *
   * The scan reads the **file**, not the index. Searching materialized rows
   * would search a truncated preview of each record (`DEEP_REASONING.md` C33)
   * and miss matches that are genuinely there, which is worse than having no
   * search at all because the user believes the answer.
   */
  find: {
    params: { needle: string; caseSensitive: boolean };
    result: Record<string, never>;
  };

  /** Abandon the search in progress. Idempotent. */
  findStop: { params: Record<string, never>; result: Record<string, never> };

  /**
   * Which root row contains a byte offset.
   *
   * `null` for a byte before the first row. What "go to offset" is built on,
   * and what M3's jump-to-error will use to turn a parse failure's byte into a
   * place in the tree.
   */
  locate: { params: { offset: number }; result: { row: number | null } };

  /**
   * Check the document for well-formedness.
   *
   * Starts a pass and returns immediately; results arrive as `validated`
   * events. Uses the same lexer and grammar walk that built the index, so an
   * error's offset is an offset the tree can be sent to — a validator that
   * agreed with itself but not with the viewer would be worse than none.
   */
  validate: { params: Record<string, never>; result: Record<string, never> };

  /** Abandon the validation pass in progress. Idempotent. */
  validateStop: { params: Record<string, never>; result: Record<string, never> };

  /** Close the current file and release every index built over it. */
  close: { params: Record<string, never>; result: Record<string, never> };
}

/** Name of a declared call. */
export type Method = keyof Calls;

/** Parameters of call `M`. */
export type Params<M extends Method> = Calls[M]['params'];

/** Result of call `M`. */
export type Result<M extends Method> = Calls[M]['result'];

/** How much of a file is enough to tell single-document from NDJSON. */
export const SNIFF_PREFIX_BYTES = 64 * 1024;

/** A request in flight, UI → Worker. */
export interface RequestEnvelope<M extends Method = Method> {
  /** Correlates the response. Monotonic per client, never reused. */
  id: number;
  method: M;
  params: Params<M>;
}

/** The answer to exactly one {@link RequestEnvelope}, Worker → UI. */
export type ResponseEnvelope<M extends Method = Method> =
  | { id: number; ok: true; result: Result<M> }
  | { id: number; ok: false; error: ProtocolError };

/**
 * Unsolicited Worker → UI messages.
 *
 * Distinguished from responses by having no `id`.
 *
 * `progress` is an event rather than the result of a call because the indexing
 * loop belongs to the Worker. Driving it from the UI would put a round-trip
 * between every 4 MB of a 500 MB file — 125 of them — to communicate something
 * the UI only ever reacts to.
 */
export type WorkerEvent =
  | { kind: 'ready'; core: string; protocol: number }
  | { kind: 'fatal'; error: ProtocolError }
  | {
      kind: 'progress';
      /** Bytes indexed so far. */
      consumed: number;
      /** Bytes in the file. */
      total: number;
      /** Root rows found so far — already paintable. */
      rows: number;
      /** Whether indexing has stopped, for any reason. */
      done: boolean;
      /** Why, when it is worth saying: a malformed document, or a cancel. The
       * rows found before that point are still real rows (C6). */
      stopped?: 'malformed' | 'cancelled' | 'error';
      /** Present when `stopped` is `'error'`. */
      error?: ProtocolError;
      /** What the engine occupies right now. Carried on the event that already
       * fires per batch rather than polled, so the readout costs no round
       * trips of its own. */
      usage: Usage;
    }
  | {
      kind: 'found';
      /** Which search these results belong to. Answers to a superseded search
       * are discarded rather than merged into the current one's list. */
      search: number;
      /** Row indices discovered by this step only — the UI appends. Sending
       * the whole growing list every few milliseconds is the allocation this
       * design exists to avoid, one layer up from C43. */
      rows: Float64Array;
      /** Matches so far, including any not yet resolvable to a row. */
      matches: number;
      /** Matches lying beyond the indexed region, which have no row to go to.
       * Zero unless indexing was cancelled — reported rather than dropped, so
       * a count of 12 never accompanies a list of 9. */
      pending: number;
      /** Bytes scanned so far. */
      scanned: number;
      /** Bytes to scan in total. */
      total: number;
      /** Whether the scan has stopped. */
      done: boolean;
      /** Whether it stopped at the match cap rather than the end of the file —
       * the difference between "1,024 matches" and "10,000+ matches". */
      limited: boolean;
      /** Present if the scan failed; the matches found before it still stand. */
      error?: ProtocolError;
    }
  | {
      kind: 'validated';
      /** Which pass these belong to, so a superseded one is discarded. */
      pass: number;
      /** Errors found by this step only — the UI appends. */
      problems: Problem[];
      /** Errors found in total, which may exceed what has been reported. */
      total: number;
      /** Bytes checked, and bytes there are. */
      checked: number;
      bytes: number;
      /** Top-level values checked — records, for NDJSON. */
      values: number;
      /** Whether the pass has finished. */
      done: boolean;
      /** Present if the pass could not read the file. */
      error?: ProtocolError;
    };

/** One syntax error, with everywhere it can be pointed at. */
export interface Problem {
  /** Byte offset of the offending token. */
  offset: number;
  /** 1-based line. */
  line: number;
  /** 1-based column, in bytes. */
  column: number;
  /** The row it belongs to, or `null` for a byte before the first row. */
  row: number | null;
  /** What went wrong, phrased for a person. */
  message: string;
}

/** The indexing report, named so the Worker can build one before posting it. */
export type ProgressEvent = Extract<WorkerEvent, { kind: 'progress' }>;

/** One instalment of search results. */
export type FoundEvent = Extract<WorkerEvent, { kind: 'found' }>;

/** Anything the Worker may post to the UI. */
export type FromWorker = ResponseEnvelope | WorkerEvent;

/** Narrow a worker message to an event. */
export function isEvent(message: FromWorker): message is WorkerEvent {
  return !('id' in message);
}

/**
 * The `ArrayBuffer`s in a payload that should move rather than copy.
 *
 * Used in both directions: a 64 KiB sniff prefix on the way in, and a packed
 * row block on the way back. The sender's buffer is detached afterwards —
 * callers must not reuse it, which is why every call site hands over a buffer
 * it has just produced and does not keep.
 */
export function transferables(payload: object): Transferable[] {
  return Object.values(payload).filter(
    (value): value is ArrayBuffer => value instanceof ArrayBuffer,
  );
}

/** Coerce an unknown thrown value into a transportable {@link ProtocolError}. */
export function toProtocolError(thrown: unknown, message: string): ProtocolError {
  if (thrown instanceof Error) {
    return { message, cause: `${thrown.name}: ${thrown.message}` };
  }
  return { message, cause: String(thrown) };
}
