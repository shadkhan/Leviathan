/**
 * The Worker that hosts the WASM engine.
 *
 * Everything expensive happens here. The UI thread never parses, never touches
 * file bytes, and never blocks — that is the entire premise of the product, and
 * this file is where it is enforced. See SPEC §2.4.
 *
 * Responsibilities, and nothing beyond them:
 *
 * 1. Instantiate the WASM module once, from a bundled asset (MV3 forbids remote
 *    code, and privacy forbids uploading anything, so the `.wasm` ships in the
 *    extension package).
 * 2. Own the open file and answer the engine's byte-range reads from it.
 * 3. Answer {@link RequestEnvelope}s from a typed dispatch table.
 * 4. Announce readiness, progress, or failure.
 *
 * ## The one thing worth reading before changing anything here
 *
 * The engine reads bytes **synchronously**, through {@link BlobReader}. That is
 * only possible because `FileReaderSync` exists, and it exists only in a
 * Worker. It blocks this thread for the duration of a read, which is exactly
 * what we want: the main thread is untouched, and the alternative — inverting
 * the engine so it returns "I need bytes at X" and resumes later — would put an
 * async hop inside the lexer's inner loop.
 */

import init, {
  Document,
  coreVersion,
  echo,
  rowLayoutVersion,
  sniffFormat,
} from '../wasm/leviathan_wasm.js';
import {
  PROTOCOL_VERSION,
  toProtocolError,
  transferables,
  type Format,
  type FromWorker,
  type Method,
  type Params,
  type ProgressEvent,
  type RequestEnvelope,
  type Result,
  type WorkerEvent,
} from '../protocol/index.js';
import { ROW_LAYOUT_VERSION } from '../protocol/rows.js';

/**
 * Where the `.wasm` lives, resolved against this script rather than hardcoded.
 *
 * The build emits `worker.js` and `leviathan_wasm_bg.wasm` side by side in
 * `dist/`, so this resolves to a `chrome-extension://` URL of our own package.
 * Deliberately not a network URL: MV3 rejects remote code, and a JSON viewer
 * that phones home would defeat the reason people reach for a local tool.
 */
const WASM_URL = new URL('leviathan_wasm_bg.wasm', self.location.href);

/**
 * A synchronous byte-range reader over a `File`, for the engine to pull from.
 *
 * `slice` is free — it makes a view, it does not read — so the only real cost
 * is `readAsArrayBuffer`, and the engine is built to call it once per screen of
 * rows rather than once per row (`DEEP_REASONING.md` C32).
 *
 * A short read at the end of the file is normal and is not signalled: the
 * engine asks speculatively past the end as a matter of course, and wants the
 * bytes that exist rather than a diagnostic (C40).
 */
class BlobReader {
  readonly #file: File;
  readonly #reader = new FileReaderSync();

  constructor(file: File) {
    this.#file = file;
  }

  read(start: number, length: number): Uint8Array {
    const from = Math.max(0, Math.min(start, this.#file.size));
    const to = Math.min(from + Math.max(0, length), this.#file.size);
    if (to <= from) {
      return new Uint8Array(0);
    }
    return new Uint8Array(this.#reader.readAsArrayBuffer(this.#file.slice(from, to)));
  }
}

/** The file currently open, if any. */
let open: Document | undefined;

/**
 * The same reader the engine pulls through, kept so the host can read a range
 * itself — which is what "copy value" is: bytes the engine already located,
 * fetched by offset rather than re-derived.
 */
let reader: BlobReader | undefined;

/** Set by `cancel`, read by the indexing loop between batches. */
let cancelled = false;

const DECODER = new TextDecoder();

function requireOpen(): Document {
  if (!open) {
    throw new Error('No file is open. Call "open" first.');
  }
  return open;
}

/** One handler per declared call, checked exhaustively by the compiler. */
type Handlers = {
  [M in Method]: (params: Params<M>) => Result<M>;
};

const handlers: Handlers = {
  echo: ({ value }) => ({ value: echo(value) }),

  version: () => ({ core: coreVersion(), protocol: PROTOCOL_VERSION }),

  /**
   * The cast is the one place the boundary is not type-safe: Rust returns a
   * `String`, and TypeScript has to be told which strings are possible. The
   * `Format` union and `leviathan_core::Format::as_str` are kept in step by
   * hand; there are four values and they are covered by a test on each side.
   */
  sniff: ({ prefix }) => ({ format: sniffFormat(new Uint8Array(prefix)) as Format }),

  open: ({ file }) => {
    closeCurrent();
    cancelled = false;

    reader = new BlobReader(file);
    const document = new Document(file.size, reader);
    open = document;
    // Deliberately not awaited: the answer to `open` is the format, which is
    // known now, and the tree can be painted from a partial index long before
    // this finishes.
    void indexToEnd(document);

    return { format: document.format as Format, size: document.size };
  },

  cancel: () => {
    cancelled = true;
    const document = requireOpen();
    return { consumed: document.indexedBytes, rows: document.rowCount(null) };
  },

  rowCount: ({ container }) => ({ count: requireOpen().rowCount(container) }),

  rows: ({ container, start, count }) => {
    const packed = requireOpen().rows(container, start, count);
    // `packed` is a fresh `Uint8Array` copied out of linear memory by
    // wasm-bindgen, so its buffer is ours alone to transfer. Handing over a
    // view onto WASM memory instead would be a use-after-free the moment the
    // heap grew (C5).
    return { packed: packed.buffer as ArrayBuffer };
  },

  expand: ({ offset }) => {
    const step = requireOpen().expandStep(offset);
    try {
      return { children: step.children, done: step.done, complete: step.complete };
    } finally {
      // A wasm-bindgen struct is a pointer into linear memory with a JS wrapper
      // around it. Freeing it is not optional bookkeeping — left to the
      // finalizer, an expand-per-frame UI leaks the WASM heap at exactly the
      // rate the user scrolls.
      step.free();
    }
  },

  /**
   * A value's bytes, decoded as text.
   *
   * Bounded twice over: by the value's own end when the engine knows it, and by
   * the caller's `limit` when it does not. Reading "to the end of the value"
   * without a limit is how a copy of one row becomes a 400 MB string on a
   * thread that is supposed to stay responsive.
   */
  text: ({ start, end, limit }) => {
    requireOpen();
    if (!reader) {
      throw new Error('No file is open. Call "open" first.');
    }
    const wanted = end === null ? limit : Math.min(end - start, limit);
    const bytes = reader.read(start, Math.max(0, wanted));
    return {
      text: DECODER.decode(bytes),
      truncated: end === null ? bytes.length >= limit : end - start > limit,
    };
  },

  forget: ({ offset }) => {
    requireOpen().forget(offset);
    return {};
  },

  close: () => {
    closeCurrent();
    return {};
  },
};

/**
 * Index the whole file, reporting as it goes.
 *
 * Yields to the message loop between batches — with `setTimeout`, not
 * `queueMicrotask`, because a microtask does not let a queued `postMessage`
 * through and a cancel would sit unnoticed until the file was finished.
 */
async function indexToEnd(document: Document): Promise<void> {
  for (;;) {
    if (document !== open) {
      return; // A newer file replaced this one; stop quietly.
    }
    if (cancelled) {
      emit(halted(document, 'cancelled'));
      return;
    }

    let done: boolean;
    const step = (() => {
      try {
        return document.indexStep();
      } catch (thrown) {
        emit({
          ...halted(document, 'error'),
          error: toProtocolError(thrown, 'Indexing stopped: the file could not be read.'),
        });
        return undefined;
      }
    })();
    if (!step) {
      return;
    }

    try {
      done = step.done;
      emit({
        kind: 'progress',
        consumed: step.consumed,
        total: step.total,
        rows: step.rows,
        done,
        ...(step.malformed ? { stopped: 'malformed' as const } : {}),
      });
    } finally {
      step.free();
    }

    if (done) {
      return;
    }

    await new Promise<void>((resolve) => {
      setTimeout(resolve, 0);
    });
  }
}

/** The final progress event for an index that stopped without finishing. */
function halted(document: Document, why: 'cancelled' | 'error'): ProgressEvent {
  return {
    kind: 'progress',
    consumed: document.indexedBytes,
    total: document.size,
    rows: document.rowCount(null),
    done: true,
    stopped: why,
  };
}

function closeCurrent(): void {
  cancelled = true;
  reader = undefined;
  open?.free();
  // Cleared before the loop can observe it: the loop's first act after a yield
  // is to check that its document is still the open one, so freeing here can
  // never race with a step in flight.
  open = undefined;
}

/**
 * Resolves once WASM is instantiated; started by the first request.
 *
 * The UI is allowed to call immediately — a rule saying "wait for `ready`
 * before you may speak" is exactly the kind of ordering constraint a later
 * refactor breaks silently. Chaining each request onto this promise removes the
 * rule instead of documenting it, and promise ordering preserves FIFO for free.
 */
let ready: Promise<void> | undefined;

function post(message: FromWorker, transfer: Transferable[] = []): void {
  self.postMessage(message, { transfer });
}

function emit(event: WorkerEvent): void {
  post(event);
}

/**
 * Run one request and answer it.
 *
 * A handler that throws becomes an error response, never an unhandled rejection
 * and never silence: a request without an answer would hang the UI's pending
 * map forever.
 */
function answer(request: RequestEnvelope): void {
  const handler = handlers[request.method] as
    | ((params: Params<Method>) => Result<Method>)
    | undefined;

  if (!handler) {
    post({
      id: request.id,
      ok: false,
      error: {
        message: `Unknown method "${request.method}".`,
        cause: 'The UI and Worker bundles are probably from different builds.',
      },
    });
    return;
  }

  try {
    const result = handler(request.params);
    post({ id: request.id, ok: true, result }, transferables(result));
  } catch (thrown) {
    post({
      id: request.id,
      ok: false,
      error: toProtocolError(thrown, `Call "${request.method}" failed.`),
    });
  }
}

async function start(): Promise<void> {
  await init({ module_or_path: WASM_URL });

  // The row buffer is read by byte offset on the far side, so a layout skew
  // between the `.wasm` and the bundle is not a type error — it is silently
  // wrong rows. Refuse to start instead.
  const engineLayout = rowLayoutVersion();
  if (engineLayout !== ROW_LAYOUT_VERSION) {
    throw new Error(
      `Row layout mismatch: the bundle reads v${ROW_LAYOUT_VERSION}, the .wasm writes v${engineLayout}.`,
    );
  }

  emit({ kind: 'ready', core: coreVersion(), protocol: PROTOCOL_VERSION });
}

self.onmessage = (message: MessageEvent<RequestEnvelope>): void => {
  const request = message.data;

  // Guard the runtime shape: `onmessage` accepts whatever anyone posts, and a
  // malformed message must not take the Worker down with it.
  if (typeof request?.id !== 'number' || typeof request?.method !== 'string') {
    emit({
      kind: 'fatal',
      error: { message: 'Worker received a message that is not a request.' },
    });
    return;
  }

  // Started once, by whoever speaks first. If it fails, `fatal` is emitted once
  // and the promise still settles — so every queued request gets an error
  // response rather than hanging in the client's pending map.
  ready ??= start().catch((thrown: unknown) => {
    emit({
      kind: 'fatal',
      error: toProtocolError(
        thrown,
        'Could not start the Leviathan engine. The bundled WebAssembly module failed to load.',
      ),
    });
  });

  void ready.then(() => {
    answer(request);
  });
};
