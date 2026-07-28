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
export const PROTOCOL_VERSION = 1;

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
 * Every call the UI can make, as `method → { params, result }`.
 *
 * This is the single source of truth for the request surface. M1 adds `index`,
 * `getRows`, and `cancel` here.
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
 * Distinguished from responses by having no `id`. M1 adds
 * `{ kind: 'progress' }` here for cancellable indexing.
 */
export type WorkerEvent =
  | { kind: 'ready'; core: string; protocol: number }
  | { kind: 'fatal'; error: ProtocolError };

/** Anything the Worker may post to the UI. */
export type FromWorker = ResponseEnvelope | WorkerEvent;

/** Narrow a worker message to an event. */
export function isEvent(message: FromWorker): message is WorkerEvent {
  return !('id' in message);
}

/**
 * The `ArrayBuffer`s in `params` that should move rather than copy.
 *
 * Transferring is why a 64 KiB sniff prefix costs nothing, and it is the same
 * mechanism M1 uses for index row blocks. The sender's buffer is detached
 * afterwards — callers must not reuse it, which is why every call site slices a
 * fresh buffer off the source.
 */
export function transferables<M extends Method>(params: Params<M>): Transferable[] {
  return Object.values(params).filter(
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
