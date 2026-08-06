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
} from "../wasm/leviathan_wasm.js";
import {
  PROTOCOL_VERSION,
  toProtocolError,
  transferables,
  type Format,
  type FromWorker,
  type Method,
  type Params,
  type Problem,
  type ProgressEvent,
  type RequestEnvelope,
  type SearchMode,
  type Usage,
  type Result,
  type WorkerEvent,
} from "../protocol/index.js";
import { ROW_LAYOUT_VERSION } from "../protocol/rows.js";

/**
 * Where the `.wasm` lives, resolved against this script rather than hardcoded.
 *
 * The build emits `worker.js` and `leviathan_wasm_bg.wasm` side by side in
 * `dist/`, so this resolves to a `chrome-extension://` URL of our own package.
 * Deliberately not a network URL: MV3 rejects remote code, and a JSON viewer
 * that phones home would defeat the reason people reach for a local tool.
 */
const WASM_URL = new URL("leviathan_wasm_bg.wasm", self.location.href);

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
    return new Uint8Array(
      this.#reader.readAsArrayBuffer(this.#file.slice(from, to)),
    );
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

/** The WASM instance, held for its linear memory. */
let instance: Awaited<ReturnType<typeof init>> | undefined;

/** What the engine occupies right now. */
function usage(document: Document | undefined): Usage {
  return {
    index: document?.heapBytes ?? 0,
    heap: instance?.memory.buffer.byteLength ?? 0,
  };
}

/** Set by `cancel`, read by the indexing loop between batches. */
let cancelled = false;

/**
 * Which search is current.
 *
 * Bumped by every `find` and every `findStop`, so a scan loop can tell whether
 * it is still the one anyone is waiting for. Typing in the find box starts a new
 * search every keystroke, and the previous one has to notice and stop rather
 * than keep posting results into a list that has moved on.
 */
let search = 0;

/** Which validation pass is current. Same discipline as `search`. */
let pass = 0;

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
  sniff: ({ prefix }) => ({
    format: sniffFormat(new Uint8Array(prefix)) as Format,
  }),

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
      return {
        children: step.children,
        done: step.done,
        complete: step.complete,
        usage: usage(open),
      };
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

  find: ({ needle, caseSensitive, mode }) => {
    const document = requireOpen();

    if (mode === "filter") {
      try {
        document.filterSet(needle);
      } catch (thrown) {
        // A syntax error is about what was typed, not about the file, so it is
        // answered here rather than posted later as if it were a finding. The
        // previous search is still cancelled: the results on screen belong to a
        // query the user has moved on from either way.
        search++;
        document.filterStop();
        return {
          error: toProtocolError(thrown, "That filter could not be parsed.")
            .message,
        };
      }
      document.filterStart();
    } else {
      document.findStart(needle, caseSensitive, undefined);
    }

    // Same shape as `open`: the answer is "started", and the results arrive as
    // events. Awaiting the scan would hold the response for as long as it takes
    // to read the file, which on the file this product exists for is the whole
    // point of not doing it that way.
    void searchToEnd(document, ++search, mode);
    return { error: null };
  },

  findStop: () => {
    search++;
    open?.findStop();
    open?.filterStop();
    return {};
  },

  locate: ({ offset }) => ({ row: requireOpen().rowAtByte(offset) ?? null }),

  validate: () => {
    const document = requireOpen();
    document.validateStart();
    void checkToEnd(document, ++pass);
    return {};
  },

  validateStop: () => {
    pass++;
    return {};
  },

  schema: ({ source }) => {
    const document = requireOpen();
    const unsupported = document.schemaSet(source);
    document.schemaStart();
    void checkToEnd(document, ++pass, true);
    return {
      unsupported: unsupported === "" ? [] : unsupported.split("\u0001"),
    };
  },

  dedup: ({ keys, elements }) => {
    const document = requireOpen();
    document.dedupStart(keys, elements);
    void dedupToEnd(document, ++pass);
    return {};
  },

  exportFormats: () => ({
    formats: ["json", "json-pretty", "ndjson", "csv"],
  }),

  exportStep: ({ start }) => {
    const document = requireOpen();
    if (start) {
      document.exportStart(start.format, new Float64Array(start.rows));
    }
    const step = document.exportStep();
    try {
      // The chunk is copied out of WASM memory by wasm-bindgen already, so this
      // buffer is ours to transfer rather than copy again.
      // `slice` on a `Uint8Array` gives a copy with its own `ArrayBuffer`,
      // which is what the transfer list needs. Reaching through `.buffer` types
      // as `ArrayBuffer | SharedArrayBuffer` and would be a view into WASM
      // memory besides — the thing C5 says never to hand out.
      const chunk = step.chunk.slice();
      return {
        chunk: chunk.buffer as ArrayBuffer,
        records: step.records,
        done: step.done,
        truncated: step.truncated,
      };
    } finally {
      step.free();
    }
  },

  exportStop: () => {
    open?.exportStop();
    return {};
  },

  close: () => {
    closeCurrent();
    return {};
  },
};

/**
 * Check the whole file, posting errors as they are found.
 *
 * Same shape as the indexing and search loops, for the same reason: a pass over
 * 500 MB is seconds of work, and a Worker that stops answering during it cannot
 * be cancelled, cannot report progress, and cannot serve the rows the user is
 * still scrolling through while it runs.
 */
async function checkToEnd(
  document: Document,
  id: number,
  schema = false,
): Promise<void> {
  for (;;) {
    if (document !== open || id !== pass) {
      return; // Superseded, stopped, or the file was closed.
    }

    let done: boolean;
    try {
      const step = schema ? document.schemaStep() : document.validateStep();
      try {
        done = step.done;
        // Four doubles per error, and the messages in one string — unpacked
        // here so the UI never sees the wire format.
        const positions = step.positions;
        // U+0001, matching `Validated::messages`. Written as an escape rather
        // than as the character itself, which is invisible in every editor.
        const messages =
          step.messages === "" ? [] : step.messages.split("\u0001");
        const problems = messages.map((message, at) => ({
          offset: positions[at * 4] ?? 0,
          line: positions[at * 4 + 1] ?? 1,
          column: positions[at * 4 + 2] ?? 1,
          row:
            (positions[at * 4 + 3] ?? -1) < 0
              ? null
              : (positions[at * 4 + 3] as number),
          message,
        }));

        emit({
          kind: "validated",
          pass: id,
          problems,
          total: step.errors,
          checked: step.checked,
          bytes: step.total,
          values: step.values,
          done,
        });
      } finally {
        step.free();
      }
    } catch (thrown) {
      emit({
        kind: "validated",
        pass: id,
        problems: [],
        total: 0,
        checked: 0,
        bytes: document.size,
        values: 0,
        done: true,
        error: toProtocolError(
          thrown,
          "Validation stopped: the file could not be read.",
        ),
      });
      return;
    }

    if (done) {
      return;
    }

    await new Promise<void>((resolve) => {
      setTimeout(resolve, 0);
    });
  }
}

/**
 * Walk the file for duplicates, posting them as they are found.
 *
 * The same loop shape as {@link checkToEnd}, reporting into the same `validated`
 * event: a duplicate is a finding with a location, which is what that event
 * carries. The repeat's offset is the one reported, because that is the member
 * you would delete; the first occurrence is named in the message so both are
 * reachable.
 */
async function dedupToEnd(document: Document, id: number): Promise<void> {
  for (;;) {
    if (document !== open || id !== pass) {
      return; // Superseded, stopped, or the file was closed.
    }

    let done: boolean;
    try {
      const step = document.dedupStep();
      try {
        done = step.done;
        const positions = step.positions;
        const messages = step.messages === "" ? [] : step.messages.split("");
        const problems: Problem[] = messages.map((entry, at) => {
          const [kind, what] = entry.split("");
          const first = positions[at * 4] ?? 0;
          const second = positions[at * 4 + 2] ?? 0;
          const row = positions[at * 4 + 3] ?? -1;
          const named = kind === "key" ? `key "${what}"` : `element ${what}`;
          return {
            offset: second,
            // A duplicate has no line of its own to report: it is a fact about
            // two places, and both are byte offsets. Zero means "no line", and
            // the renderer shows the offset instead.
            line: 0,
            column: 0,
            row: row < 0 ? null : row,
            message: `duplicate ${named} — first at byte ${first.toLocaleString()}`,
          };
        });

        emit({
          kind: "validated",
          pass: id,
          problems,
          total: step.found,
          checked: step.walked,
          bytes: step.total,
          values: step.keys + step.elements,
          done,
        });
      } finally {
        step.free();
      }
    } catch (thrown) {
      emit({
        kind: "validated",
        pass: id,
        problems: [],
        total: 0,
        checked: 0,
        bytes: document.size,
        values: 0,
        done: true,
        error: toProtocolError(
          thrown,
          "The duplicate check stopped: the file could not be read.",
        ),
      });
      return;
    }

    if (done) {
      return;
    }

    await new Promise<void>((resolve) => {
      setTimeout(resolve, 0);
    });
  }
}

/**
 * Scan the file for the current needle, posting results as they are found.
 *
 * Structured exactly like {@link indexToEnd}, and for the same reason: a scan of
 * a 500 MB file is seconds of work, and the alternative to yielding between
 * batches is a Worker that stops answering — including stopping answering the
 * keystroke that would have replaced this search with a better one.
 */
async function searchToEnd(
  document: Document,
  id: number,
  mode: SearchMode,
): Promise<void> {
  for (;;) {
    if (document !== open || id !== search) {
      return; // Superseded, stopped, or the file was closed. Say nothing.
    }

    let done: boolean;
    try {
      // Both steppers return the same `Found`, so everything past this line is
      // identical for a byte scan and a record filter — which is the point of
      // having reused it.
      const step =
        mode === "filter" ? document.filterStep() : document.findStep();
      try {
        done = step.done;
        emit({
          kind: "found",
          search: id,
          rows: step.rows,
          matches: step.matches,
          pending: step.pending,
          scanned: step.scanned,
          total: document.size,
          done,
          limited: step.limited,
        });
      } finally {
        step.free();
      }
    } catch (thrown) {
      emit({
        kind: "found",
        search: id,
        rows: new Float64Array(0),
        matches: 0,
        pending: 0,
        scanned: 0,
        total: document.size,
        done: true,
        limited: false,
        error: toProtocolError(
          thrown,
          "Search stopped: the file could not be read.",
        ),
      });
      return;
    }

    if (done) {
      return;
    }

    await new Promise<void>((resolve) => {
      setTimeout(resolve, 0);
    });
  }
}

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
      emit(halted(document, "cancelled"));
      return;
    }

    let done: boolean;
    const step = (() => {
      try {
        return document.indexStep();
      } catch (thrown) {
        // A WebAssembly trap is not a read failure, and saying it is sends the
        // user to check their disk when the engine ran out of address space.
        // The index path returns `exhausted` rather than trapping, so anything
        // that still gets here is an allocation somewhere else — but the
        // message has to be honest about which of the two it might be.
        const trapped = thrown instanceof WebAssembly.RuntimeError;
        emit({
          ...halted(document, trapped ? "exhausted" : "error"),
          error: toProtocolError(
            thrown,
            trapped
              ? "Indexing stopped: the engine ran out of memory. " +
                  "The rows found so far are still browsable."
              : "Indexing stopped: the file could not be read.",
          ),
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
        kind: "progress",
        consumed: step.consumed,
        total: step.total,
        rows: step.rows,
        done,
        ...(step.malformed ? { stopped: "malformed" as const } : {}),
        ...(step.exhausted ? { stopped: "exhausted" as const } : {}),
        usage: usage(document),
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
function halted(
  document: Document,
  why: "cancelled" | "error" | "exhausted",
): ProgressEvent {
  return {
    kind: "progress",
    consumed: document.indexedBytes,
    total: document.size,
    rows: document.rowCount(null),
    done: true,
    stopped: why,
    usage: usage(document),
  };
}

function closeCurrent(): void {
  cancelled = true;
  // Any scan in flight belongs to a file that is about to be freed. Bumping the
  // ids is what makes their next iteration return instead of calling into a
  // `Document` that no longer exists.
  search++;
  pass++;
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
    ((params: Params<Method>) => Result<Method>) | undefined;

  if (!handler) {
    post({
      id: request.id,
      ok: false,
      error: {
        message: `Unknown method "${request.method}".`,
        cause: "The UI and Worker bundles are probably from different builds.",
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
  // The instance is kept for its `memory`: requirement 9 asks for the engine's
  // real footprint, and linear memory is it — the number the browser actually
  // reserved, not an estimate of what is in it.
  instance = await init({ module_or_path: WASM_URL });

  // The row buffer is read by byte offset on the far side, so a layout skew
  // between the `.wasm` and the bundle is not a type error — it is silently
  // wrong rows. Refuse to start instead.
  const engineLayout = rowLayoutVersion();
  if (engineLayout !== ROW_LAYOUT_VERSION) {
    throw new Error(
      `Row layout mismatch: the bundle reads v${ROW_LAYOUT_VERSION}, the .wasm writes v${engineLayout}.`,
    );
  }

  emit({ kind: "ready", core: coreVersion(), protocol: PROTOCOL_VERSION });
}

self.onmessage = (message: MessageEvent<RequestEnvelope>): void => {
  const request = message.data;

  // Guard the runtime shape: `onmessage` accepts whatever anyone posts, and a
  // malformed message must not take the Worker down with it.
  if (typeof request?.id !== "number" || typeof request?.method !== "string") {
    emit({
      kind: "fatal",
      error: { message: "Worker received a message that is not a request." },
    });
    return;
  }

  // Started once, by whoever speaks first. If it fails, `fatal` is emitted once
  // and the promise still settles — so every queued request gets an error
  // response rather than hanging in the client's pending map.
  ready ??= start().catch((thrown: unknown) => {
    emit({
      kind: "fatal",
      error: toProtocolError(
        thrown,
        "Could not start the Leviathan engine. The bundled WebAssembly module failed to load.",
      ),
    });
  });

  void ready.then(() => {
    answer(request);
  });
};
