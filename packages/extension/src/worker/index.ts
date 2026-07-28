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
 * 2. Answer {@link RequestEnvelope}s from a typed dispatch table.
 * 3. Announce readiness, or announce failure loudly.
 */

import init, { coreVersion, echo, sniffFormat } from '../wasm/leviathan_wasm.js';
import {
  PROTOCOL_VERSION,
  toProtocolError,
  type Format,
  type FromWorker,
  type Method,
  type Params,
  type RequestEnvelope,
  type Result,
  type WorkerEvent,
} from '../protocol/index.js';

/**
 * Where the `.wasm` lives, resolved against this script rather than hardcoded.
 *
 * The build emits `worker.js` and `leviathan_wasm_bg.wasm` side by side in
 * `dist/`, so this resolves to a `chrome-extension://` URL of our own package.
 * Deliberately not a network URL: MV3 rejects remote code, and a JSON viewer
 * that phones home would defeat the reason people reach for a local tool.
 */
const WASM_URL = new URL('leviathan_wasm_bg.wasm', self.location.href);

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
};

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
    post({ id: request.id, ok: true, result: handler(request.params) });
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
