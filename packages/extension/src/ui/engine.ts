/**
 * The UI thread's typed handle on the Worker.
 *
 * This is the only place in the UI that knows a Worker exists. Everything else
 * calls {@link Engine.call} and gets a promise, which keeps the "no parsing on
 * the main thread" rule structural rather than aspirational — there is no API
 * here that could do work locally even if someone wanted it to.
 */

import {
  PROTOCOL_VERSION,
  isEvent,
  transferables,
  type FromWorker,
  type Method,
  type Params,
  type ProtocolError,
  type RequestEnvelope,
  type Result,
  type WorkerEvent,
} from '../protocol/index.js';

/** A failure that crossed the boundary, rethrown on the UI side with its cause. */
export class EngineError extends Error {
  override readonly name = 'EngineError';

  constructor(error: ProtocolError) {
    super(error.message, error.cause === undefined ? undefined : { cause: error.cause });
  }
}

interface Pending {
  resolve: (result: never) => void;
  reject: (error: EngineError) => void;
}

export class Engine {
  readonly #worker: Worker;
  readonly #pending = new Map<number, Pending>();
  #nextId = 1;

  /**
   * @param worker  The Worker hosting the engine.
   * @param onEvent Unsolicited worker messages — readiness, and from M1,
   *                indexing progress.
   */
  constructor(worker: Worker, onEvent: (event: WorkerEvent) => void) {
    this.#worker = worker;
    this.#worker.onmessage = ({ data }: MessageEvent<FromWorker>): void => {
      if (isEvent(data)) {
        // A fatal event means the engine will never answer anything. Failing
        // the outstanding calls is the difference between an error message and
        // a spinner that turns forever.
        if (data.kind === 'fatal') {
          this.#failAll(data.error);
        }
        onEvent(data);
        return;
      }

      const pending = this.#pending.get(data.id);
      if (!pending) {
        return; // A response to a call we already failed. Nothing to do.
      }
      this.#pending.delete(data.id);
      if (data.ok) {
        pending.resolve(data.result as never);
      } else {
        pending.reject(new EngineError(data.error));
      }
    };

    // An error the Worker could not report through the protocol: a syntax error
    // in the bundle, or an import that 404s. Without this it fails silently.
    this.#worker.onerror = (event: ErrorEvent): void => {
      this.#failAll({
        message: 'The Leviathan worker crashed.',
        cause: event.message,
      });
    };
  }

  /** Invoke `method` in the Worker. Rejects with {@link EngineError}. */
  call<M extends Method>(method: M, params: Params<M>): Promise<Result<M>> {
    const id = this.#nextId++;
    const request: RequestEnvelope<M> = { id, method, params };

    return new Promise<Result<M>>((resolve, reject) => {
      this.#pending.set(id, { resolve: resolve as Pending['resolve'], reject });
      this.#worker.postMessage(request, transferables<M>(params));
    });
  }

  /**
   * Confirm the Worker is running the engine this bundle was built against.
   *
   * A stale `dist/` — TypeScript rebuilt, `.wasm` not — is the most likely
   * confusing bug in this project. This turns it into one clear sentence.
   */
  async checkVersion(): Promise<{ core: string; protocol: number }> {
    const version = await this.call('version', {});
    if (version.protocol !== PROTOCOL_VERSION) {
      throw new EngineError({
        message: `Protocol mismatch: the UI speaks v${PROTOCOL_VERSION}, the worker speaks v${version.protocol}.`,
        cause: 'Rebuild the extension with `pnpm build`.',
      });
    }
    return version;
  }

  #failAll(error: ProtocolError): void {
    for (const pending of this.#pending.values()) {
      pending.reject(new EngineError(error));
    }
    this.#pending.clear();
  }
}
