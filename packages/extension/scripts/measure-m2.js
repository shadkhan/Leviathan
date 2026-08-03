/*
 * M2 exit-criterion measurement, run from the viewer's DevTools console.
 *
 * SPEC §M2 asks for four numbers and this produces all four:
 *
 *   1. first rows painted        < 2 s
 *   2. longest frame on scroll   < 32 ms
 *   3. long tasks > 50 ms        zero
 *   4. index throughput (WASM)   ≥ 100 MB/s      ← also M1's last open number
 *
 * ## Usage
 *
 *   1. Load the extension and open the viewer page.
 *   2. Open DevTools → Console, paste this whole file, press Enter.
 *      It prints "armed" and then waits.
 *   3. Drop `fixtures/generated/ndjson-500.0MB.ndjson` onto the page.
 *
 * Everything after that is automatic: it times the load, waits for indexing to
 * finish, drives a scroll through 100 000 rows, and prints a Markdown table.
 * Copy that table back.
 *
 * ## What it measures, and why it measures it that way
 *
 * **First paint means a row with content in it.** A virtual list can put forty
 * placeholder rows on screen in one frame and look instant while the engine has
 * answered nothing — so this waits for the first `.tree-row` that is *not*
 * `data-loading`, which is a row whose bytes came back from the Worker.
 *
 * **The scroll is driven in row units, not pixels.** Above ~8 M px the list caps
 * its canvas and scales the scrollbar (`list.ts`), so a pixel is worth several
 * rows and a pixel-based scroll would silently cover the wrong distance. This
 * derives rows-per-pixel from the canvas height and moves in row terms.
 *
 * **Frame time is measured with `requestAnimationFrame` deltas** rather than
 * with the Performance panel, because the number wanted is what the *user*
 * experiences: how long between one painted frame and the next. Long tasks come
 * from `PerformanceObserver`, which sees main-thread blocking the rAF loop
 * cannot distinguish from an idle gap.
 *
 * Nothing here is shipped — `build.mjs` bundles `src/`, not `scripts/`.
 */

(() => {
  const ROWS_TO_SCROLL = 100_000;
  const SETTLE_MS = 400;

  const $ = (id) => {
    const el = document.getElementById(id);
    if (!el) throw new Error(`#${id} not found — is this the viewer page?`);
    return el;
  };

  const viewport = $('viewport');
  const canvas = $('canvas');
  const progressFill = $('progress-fill');
  const indexState = $('index-state');

  const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
  const now = () => performance.now();

  /**
   * Whether the page was ever hidden or unfocused while measuring.
   *
   * `requestAnimationFrame` stops firing for a background tab, so the next
   * delta after returning is the length of the absence, not the length of a
   * frame. A run that reported a 25 s "longest frame" alongside **zero** long
   * tasks was measuring exactly this: the thread was not busy, it was not being
   * scheduled. A frame time is only a frame time while the compositor is
   * actually asked for frames.
   */
  let interrupted = false;
  document.addEventListener('visibilitychange', () => {
    if (document.hidden) {
      interrupted = true;
    }
  });
  // Deliberately *not* `window.blur`: focusing DevTools blurs the page window,
  // and you have to focus DevTools to start this. A visible-but-unfocused page
  // still gets frames, so blur is not the signal — being hidden is.

  /** Long tasks are collected for the whole session and sliced per phase. */
  const longTasks = [];
  let observer;
  try {
    observer = new PerformanceObserver((list) => {
      for (const entry of list.getEntries()) {
        longTasks.push({ start: entry.startTime, duration: entry.duration });
      }
    });
    observer.observe({ entryTypes: ['longtask'] });
  } catch {
    console.warn('longtask observer unavailable — the long-task column will read n/a');
  }

  const tasksBetween = (from, to) =>
    longTasks.filter((t) => t.start >= from && t.start <= to).map((t) => t.duration);

  /** Resolve once `test` is true, polling on animation frames. */
  const until = (test, timeoutMs, what) =>
    new Promise((resolve, reject) => {
      const deadline = now() + timeoutMs;
      const tick = () => {
        if (test()) return resolve(now());
        if (now() > deadline) return reject(new Error(`timed out waiting for ${what}`));
        requestAnimationFrame(tick);
      };
      tick();
    });

  const painted = () => canvas.querySelector('.tree-row:not([data-loading])') !== null;
  const indexed = () => progressFill.dataset.state === 'done';
  const stopped = () => progressFill.dataset.state === 'stopped';

  /**
   * How many rows the tree actually holds, read from the status bar.
   *
   * *Not* derived from the canvas: above ~8 M px the list caps its height and
   * scales the scrollbar, so canvas ÷ row-height counts scaled units and
   * undercounts a 1.7 M-row file by twenty times. Deriving the scroll distance
   * from that made the first run cover the whole file at ~790 rows per frame
   * while believing it was doing 40.
   */
  function totalRows() {
    const text = indexState.textContent ?? '';
    const match = /([\d,]+)\s+rows/.exec(text);
    return match ? Number(match[1].replace(/,/g, '')) : 0;
  }

  function rowGeometry() {
    const rowHeight =
      Number.parseFloat(
        getComputedStyle(document.documentElement).getPropertyValue('--row-h'),
      ) || 24;
    const scrollable = viewport.scrollHeight - viewport.clientHeight;
    return { rowHeight, scrollable };
  }

  /**
   * Scroll `rows` rows while sampling every frame.
   *
   * Moves a fixed number of rows per frame — roughly a fast flick sustained for
   * as long as it takes — because a scroll that finishes in three frames says
   * nothing about the hundredth.
   */
  async function scrollTest(rows) {
    const { scrollable } = rowGeometry();
    const total = Math.max(1, totalRows());
    // Pixels per *real* row, which above the canvas cap is a fraction of one.
    const pxPerRow = scrollable / total;
    const rowsPerFrame = 40;

    viewport.scrollTop = 0;
    await sleep(SETTLE_MS);

    const frames = [];
    const from = now();
    let travelled = 0;
    let previous = now();

    await new Promise((resolve) => {
      const step = () => {
        const t = now();
        frames.push(t - previous);
        previous = t;

        if (travelled >= rows || viewport.scrollTop >= scrollable) return resolve();
        travelled += rowsPerFrame;
        viewport.scrollTop = Math.min(scrollable, travelled * pxPerRow);
        requestAnimationFrame(step);
      };
      requestAnimationFrame(step);
    });

    const to = now();
    // The first delta is the gap before the loop started, not a rendered frame.
    const samples = frames.slice(1);
    return { samples, from, to, travelled, total };
  }

  const ms = (n) => `${n.toFixed(1)} ms`;
  const pct = (arr, p) => {
    const sorted = [...arr].sort((a, b) => a - b);
    return sorted[Math.min(sorted.length - 1, Math.floor((sorted.length - 1) * p))] ?? 0;
  };

  async function run(file) {
    const t0 = now();
    console.log(`measuring: ${file.name}, ${(file.size / 1e6).toFixed(1)} MB`);

    // The viewer clears the tree and the progress bar synchronously when it
    // takes a file. Until that has happened, every signal below still describes
    // the *previous* file — which is how the first run of this script reported
    // a 0.9 ms first paint and 454,546 MB/s. Waiting for the reset costs a
    // frame or two and is the difference between measuring and inventing.
    // Progress leaving its terminal state is the signal, not an empty canvas:
    // a recycled row is parked off-screen rather than removed, so counting DOM
    // rows asks a question the renderer never promised to answer.
    await until(
      () => !indexed() && !stopped(),
      10_000,
      'the viewer to start on the new file (reload the tab and retry)',
    );

    const tPaint = await until(painted, 60_000, 'the first painted row');
    console.log(`first painted row: ${ms(tPaint - t0)}`);

    const tIndexed = await until(
      () => indexed() || stopped(),
      15 * 60_000,
      'indexing to finish',
    );
    if (stopped()) console.warn(`indexing stopped early: ${indexState.textContent}`);

    const indexSeconds = (tIndexed - t0) / 1000;
    const throughput = file.size / indexSeconds / 1e6;
    console.log(`indexed in ${indexSeconds.toFixed(2)} s — ${throughput.toFixed(0)} MB/s`);

    await sleep(SETTLE_MS);
    console.log(`scrolling ${ROWS_TO_SCROLL.toLocaleString()} rows…`);
    const scroll = await scrollTest(ROWS_TO_SCROLL);

    const worst = Math.max(...scroll.samples);
    const over32 = scroll.samples.filter((d) => d > 32).length;
    const tasks = tasksBetween(scroll.from, scroll.to);
    const loadTasks = tasksBetween(t0, tIndexed);

    // A number that cannot be true must not be printed as though it were. Every
    // previous version of this project's harness has had to learn this once
    // (DEEP_REASONING C14, C15, C23, C49); this is the instrument applying it
    // to itself.
    const implausible = [];
    if (interrupted) {
      implausible.push(
        'the tab lost focus or was hidden — rAF stops firing, so the gap on return' +
          ' is recorded as a frame. Keep this tab focused for the whole run (~70 s).',
      );
    }
    // A gap with no long task under it is the thread not being scheduled, not
    // the thread being slow. Caught even when no visibility event fired — a
    // sleeping machine reports neither.
    const unscheduled = scroll.samples.filter(
      (d) => d > 1000 && !tasks.some((t) => t > d / 2),
    );
    if (unscheduled.length > 0) {
      implausible.push(
        `${unscheduled.length} frame gap(s) over 1 s with no long task beneath them` +
          ` (worst ${ms(Math.max(...unscheduled))}) — the page was not being rendered`,
      );
    }
    if (tPaint - t0 < 5) {
      implausible.push('first paint under 5 ms — the tree was probably not cleared');
    }
    if (throughput > 10_000) {
      implausible.push(`${throughput.toFixed(0)} MB/s — faster than any disk or memcpy`);
    }
    if (scroll.total < 1000) {
      implausible.push(`only ${scroll.total} rows seen — the row count was not read`);
    }
    if (implausible.length > 0) {
      console.error('\nMEASUREMENT REJECTED:');
      for (const why of implausible) console.error(`  · ${why}`);
      console.error('Reload the viewer tab, re-paste this script, and drop the file again.\n');
      return 'measurement rejected';
    }

    const verdict = (ok) => (ok ? '✅' : '❌');
    const table = [
      '| Criterion | Target | Measured | |',
      '|---|---|---|---|',
      `| First rows painted | < 2 s | **${ms(tPaint - t0)}** | ${verdict(tPaint - t0 < 2000)} |`,
      `| Index throughput (WASM) | ≥ 100 MB/s | **${throughput.toFixed(0)} MB/s** | ${verdict(throughput >= 100)} |`,
      `| Longest frame, scrolling | < 32 ms | **${ms(worst)}** | ${verdict(worst < 32)} |`,
      `| Long tasks > 50 ms, scrolling | 0 | **${tasks.filter((d) => d > 50).length}** | ${verdict(tasks.filter((d) => d > 50).length === 0)} |`,
      '',
      `Frames sampled: ${scroll.samples.length} · median ${ms(pct(scroll.samples, 0.5))} · p95 ${ms(pct(scroll.samples, 0.95))} · over 32 ms: ${over32}`,
      `Rows traversed: ${scroll.travelled.toLocaleString()} of ${scroll.total.toLocaleString()} · ${(scroll.travelled / (scroll.to - scroll.from) * 1000).toFixed(0)} rows/s`,
      `Long tasks during load: ${loadTasks.filter((d) => d > 50).length}` +
        (loadTasks.length ? ` (worst ${ms(Math.max(...loadTasks))})` : ''),
      `File: ${file.name}, ${(file.size / 1e6).toFixed(1)} MB · row height ${rowGeometry().rowHeight}px`,
    ].join('\n');

    console.log(`\n${table}\n`);
    try {
      await navigator.clipboard.writeText(table);
      console.log('(copied to clipboard)');
    } catch {
      console.log('(select the table above and copy it)');
    }
    return table;
  }

  // Arm both entry points: a drop anywhere, or the file picker.
  const arm = (file) => {
    if (!file) return;
    // The viewer's own handler runs on the same event; this starts its clock
    // from the same instant rather than from a later frame.
    run(file).catch((error) => console.error('measurement failed:', error));
  };

  document.addEventListener('drop', (e) => arm(e.dataTransfer?.files?.[0]), { capture: true });
  document.getElementById('file')?.addEventListener(
    'change',
    (e) => arm(e.target.files?.[0]),
    { capture: true },
  );

  console.log('armed — now drop the 500 MB fixture onto the page');
})();
