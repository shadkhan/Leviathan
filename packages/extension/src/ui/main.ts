/**
 * Viewer page entry point.
 *
 * M0's viewer is a self-check rather than a tree: the milestone is the
 * boundary, so the page shows the boundary. What it demonstrates is not the
 * button — it is that a file dropped here is read 64 KiB at a time and never
 * handed to the page whole. That constraint is the product; it is established
 * now, while it is cheap, rather than retrofitted in M2.
 */

import { SNIFF_PREFIX_BYTES, type Format, type WorkerEvent } from '../protocol/index.js';
import { Engine, EngineError } from './engine.js';

/** Look up a required element, failing loudly rather than at first null deref. */
function el<T extends HTMLElement>(id: string): T {
  const found = document.getElementById(id);
  if (!found) {
    throw new Error(`viewer.html is missing #${id} — the HTML and this bundle disagree.`);
  }
  return found as T;
}

const statusDot = el('status').querySelector<HTMLElement>('.dot');
const statusText = el('status-text');
const engineVersion = el('engine-version');

const echoForm = el<HTMLFormElement>('echo-form');
const echoInput = el<HTMLInputElement>('echo-input');
const echoResult = el<HTMLOutputElement>('echo-result');

const drop = el('drop');
const pick = el<HTMLButtonElement>('pick');
const file = el<HTMLInputElement>('file');
const paste = el<HTMLTextAreaElement>('paste');
const sniffResult = el<HTMLOutputElement>('sniff-result');

function setStatus(state: 'pending' | 'ready' | 'failed', text: string): void {
  statusDot?.setAttribute('data-state', state);
  statusText.textContent = text;
}

function show(output: HTMLOutputElement, state: 'ok' | 'err' | '', text: string): void {
  output.dataset['state'] = state;
  output.textContent = text;
}

/** Render a thrown value in a way that names the layer that failed. */
function describe(thrown: unknown): string {
  if (thrown instanceof EngineError) {
    return thrown.cause === undefined ? thrown.message : `${thrown.message} (${thrown.cause})`;
  }
  return thrown instanceof Error ? thrown.message : String(thrown);
}

/** How each detected format reads to someone who just dropped a file. */
const FORMAT_LABEL: Record<Format, string> = {
  'single-document': 'Single JSON document',
  ndjson: 'NDJSON / JSON-lines',
  empty: 'Empty — no content to read',
  unknown: 'Not JSON — nothing here starts a JSON value',
};

const worker = new Worker(new URL('worker.js', import.meta.url), {
  type: 'module',
  name: 'leviathan-engine',
});

const engine = new Engine(worker, (event: WorkerEvent) => {
  if (event.kind === 'ready') {
    setStatus('ready', 'Engine ready');
    engineVersion.textContent = `engine ${event.core} · protocol ${event.protocol}`;
  } else {
    setStatus('failed', 'Engine failed to start');
    show(sniffResult, 'err', describe(new EngineError(event.error)));
  }
});

// Confirms the loaded `.wasm` matches this bundle. Also the first message sent,
// which is what triggers instantiation in the Worker.
engine.checkVersion().catch((thrown: unknown) => {
  setStatus('failed', 'Engine failed to start');
  show(echoResult, 'err', describe(thrown));
});

echoForm.addEventListener('submit', (event) => {
  event.preventDefault();
  const value = echoInput.valueAsNumber;
  show(echoResult, '', 'calling…');

  engine.call('echo', { value }).then(
    (result) => {
      const intact = result.value === value;
      show(
        echoResult,
        intact ? 'ok' : 'err',
        intact
          ? `${result.value} — round-tripped intact`
          : `sent ${value}, received ${result.value}`,
      );
    },
    (thrown: unknown) => {
      show(echoResult, 'err', describe(thrown));
    },
  );
});

/**
 * Send a prefix to the engine and report the format.
 *
 * `prefix` is transferred, so this function is the last owner of that buffer.
 */
async function sniff(prefix: ArrayBuffer, label: string): Promise<void> {
  const bytes = prefix.byteLength;
  show(sniffResult, '', `reading ${label}…`);
  try {
    const { format } = await engine.call('sniff', { prefix });
    show(
      sniffResult,
      format === 'unknown' || format === 'empty' ? 'err' : 'ok',
      `${FORMAT_LABEL[format]} — from ${bytes.toLocaleString()} bytes of ${label}`,
    );
  } catch (thrown) {
    show(sniffResult, 'err', describe(thrown));
  }
}

/**
 * Read at most {@link SNIFF_PREFIX_BYTES} from a file.
 *
 * `File.slice` is lazy — this reads 64 KiB whether the file is 2 KB or 2 GB,
 * and it is the same mechanism the M1 `ByteRange` implementation will use to
 * service the core's re-lexing requests.
 */
async function prefixOf(source: File): Promise<ArrayBuffer> {
  return source.slice(0, SNIFF_PREFIX_BYTES).arrayBuffer();
}

async function handleFile(source: File): Promise<void> {
  await sniff(await prefixOf(source), source.name);
}

pick.addEventListener('click', () => {
  file.click();
});

file.addEventListener('change', () => {
  const chosen = file.files?.[0];
  if (chosen) {
    void handleFile(chosen);
  }
});

drop.addEventListener('click', (event) => {
  if (event.target !== pick) {
    file.click();
  }
});

drop.addEventListener('keydown', (event) => {
  if (event.key === 'Enter' || event.key === ' ') {
    event.preventDefault();
    file.click();
  }
});

drop.addEventListener('dragover', (event) => {
  event.preventDefault();
  drop.dataset['over'] = 'true';
});

for (const type of ['dragleave', 'drop'] as const) {
  drop.addEventListener(type, () => {
    delete drop.dataset['over'];
  });
}

drop.addEventListener('drop', (event) => {
  event.preventDefault();
  const dropped = event.dataTransfer?.files[0];
  if (dropped) {
    void handleFile(dropped);
  }
});

// Pasted text is bounded the same way a file is. Debounced so that typing does
// not queue a call per keystroke — the same batching discipline the M2 tree
// applies to scroll events, established here at trivial scale.
let pasteTimer: number | undefined;
paste.addEventListener('input', () => {
  clearTimeout(pasteTimer);
  pasteTimer = self.setTimeout(() => {
    const text = paste.value;
    if (text.length === 0) {
      show(sniffResult, '', '');
      return;
    }
    const encoded = new TextEncoder().encode(text.slice(0, SNIFF_PREFIX_BYTES));
    // `.slice()` guarantees an exactly-sized buffer we own and may transfer.
    void sniff(encoded.slice().buffer as ArrayBuffer, 'pasted text');
  }, 150);
});
