/**
 * A virtual list: fixed row height, recycled DOM, and a scrollbar that lies.
 *
 * The list knows nothing about JSON. It knows how many rows exist, how tall a
 * row is, and how to ask someone else to paint row *n* into an element it hands
 * over. Everything it does is in service of one number: the count of elements in
 * the document stays proportional to the *window*, never to the file.
 *
 * ## The scrollbar lies, on purpose
 *
 * A 500 MB NDJSON file is 1 772 686 rows, which at 22 px is 39 million pixels.
 * Chrome refuses to make an element taller than 33.5 million; Firefox stops
 * around 17.9 million. A tree over a five-million-element array is 110 million.
 * So the naive "spacer of `rows × height`" approach does not merely get slow at
 * the sizes this product exists for — it silently stops working, and the last
 * rows of the file become unreachable.
 *
 * The canvas is therefore capped at {@link MAX_CANVAS_PX} and the mapping from
 * scroll position to first visible row is scaled. Below the cap the scale is
 * exactly 1 and this is an ordinary virtual list; above it, one pixel of
 * scrollbar is worth several rows, which is the same bargain a hex editor or a
 * minimap makes and is why the wheel keeps working on a file whose row count
 * has no business fitting in a scrollbar. Keyboard navigation moves by *rows*
 * regardless, so precision is never actually lost — only the pointer's grip on
 * it is.
 */

/**
 * The tallest canvas we will build.
 *
 * Chosen well below every engine's limit rather than at any one of them: the
 * cost of being conservative is a slightly coarser scrollbar on files that are
 * already beyond a scrollbar's resolution, and the cost of being wrong is rows
 * that cannot be reached at all.
 */
const MAX_CANVAS_PX = 8_000_000;

export interface ListOptions {
  /** The scrolling element. */
  viewport: HTMLElement;
  /** The positioned element rows live in; its height is the scroll range. */
  canvas: HTMLElement;
  /** Row height in CSS pixels. Fixed for v1 — variable height is a v1.1 trap. */
  rowHeight: number;
  /** Extra rows painted above and below the viewport, to cover a fast flick. */
  overscan?: number;
  /** Build an empty row element. Called at most once per resident row. */
  create: () => HTMLElement;
  /** Fill an element with row `index`. Called on every repaint of that row. */
  paint: (element: HTMLElement, index: number) => void;
}

export class VirtualList {
  readonly #options: Required<ListOptions>;
  readonly #active = new Map<number, HTMLElement>();
  readonly #pool: HTMLElement[] = [];

  #count = 0;
  #first = 0;
  #last = -1;
  #frame = 0;

  constructor(options: ListOptions) {
    this.#options = { overscan: 8, ...options };

    this.#options.viewport.addEventListener('scroll', () => {
      this.#schedule();
    });

    // A window resize changes how many rows fit, and so does a panel opening.
    // `ResizeObserver` catches both; a `window.resize` listener catches only one.
    new ResizeObserver(() => {
      this.#schedule();
    }).observe(this.#options.viewport);
  }

  /** Rows in the list. */
  get count(): number {
    return this.#count;
  }

  /** First row currently in the DOM (including overscan). */
  get first(): number {
    return this.#first;
  }

  /** Last row currently in the DOM (including overscan). */
  get last(): number {
    return this.#last;
  }

  /** How many rows fit in the viewport, rounded down. */
  get pageSize(): number {
    return Math.max(1, Math.floor(this.#options.viewport.clientHeight / this.#options.rowHeight));
  }

  /**
   * Set the number of rows.
   *
   * Called constantly while a file indexes — the root grows by tens of
   * thousands of rows a second — so it does nothing unless the number changed.
   */
  setCount(count: number): void {
    if (count === this.#count) {
      return;
    }
    this.#count = count;
    this.#schedule();
  }

  /** Repaint what is on screen, in place. Cheap: no elements are created. */
  refresh(): void {
    this.#schedule();
  }

  /** Drop every row element. Used when the whole model changed underneath. */
  reset(): void {
    for (const element of this.#active.values()) {
      element.remove();
    }
    this.#active.clear();
    this.#pool.length = 0;
    this.#options.viewport.scrollTop = 0;
    this.#schedule();
  }

  /** Scroll so that row `index` is visible, moving as little as possible. */
  reveal(index: number): void {
    const { viewport, rowHeight } = this.#options;
    const top = index * rowHeight;
    const height = viewport.clientHeight;
    const scale = this.#scale();
    const virtualTop = viewport.scrollTop * scale;

    if (top < virtualTop) {
      viewport.scrollTop = top / scale;
    } else if (top + rowHeight > virtualTop + height) {
      viewport.scrollTop = (top + rowHeight - height) / scale;
    } else {
      return;
    }
    this.#schedule();
  }

  /**
   * Coalesce updates to one per frame.
   *
   * A scroll event can fire several times between paints, and expansion
   * progress can arrive faster still. Doing the work once per frame is the
   * difference between a repaint and a queue of repaints.
   */
  #schedule(): void {
    if (this.#frame !== 0) {
      return;
    }
    this.#frame = requestAnimationFrame(() => {
      this.#frame = 0;
      this.#update();
    });
  }

  /**
   * Virtual pixels per real scrollbar pixel: 1 until the row count outgrows
   * what an element is allowed to be, and greater than 1 after that.
   */
  #scale(): number {
    const { viewport, rowHeight } = this.#options;
    const totalPx = this.#count * rowHeight;
    if (totalPx <= MAX_CANVAS_PX) {
      return 1;
    }
    const height = viewport.clientHeight;
    const real = Math.max(1, MAX_CANVAS_PX - height);
    return (totalPx - height) / real;
  }

  #update(): void {
    const { viewport, canvas, rowHeight, overscan, create, paint } = this.#options;

    const totalPx = this.#count * rowHeight;
    canvas.style.height = `${Math.min(totalPx, MAX_CANVAS_PX)}px`;

    if (this.#count === 0) {
      for (const [index, element] of this.#active) {
        this.#recycle(index, element);
      }
      this.#first = 0;
      this.#last = -1;
      return;
    }

    const scale = this.#scale();
    const virtualTop = viewport.scrollTop * scale;
    const visible = Math.ceil(viewport.clientHeight / rowHeight);

    const first = Math.max(0, Math.floor(virtualTop / rowHeight) - overscan);
    const last = Math.min(this.#count - 1, first + visible + overscan * 2);

    // Rows are positioned relative to the current scroll offset rather than at
    // their absolute pixel, because above the cap those are different numbers.
    // Below the cap the two expressions are identical, so there is one path.
    const anchor = viewport.scrollTop - (virtualTop - Math.floor(virtualTop / rowHeight) * rowHeight);
    const base = Math.floor(virtualTop / rowHeight);

    for (const [index, element] of this.#active) {
      if (index < first || index > last) {
        this.#recycle(index, element);
      }
    }

    for (let index = first; index <= last; index++) {
      let element = this.#active.get(index);
      if (!element) {
        element = this.#pool.pop() ?? create();
        this.#active.set(index, element);
        if (!element.isConnected) {
          canvas.append(element);
        }
      }
      element.style.transform = `translateY(${anchor + (index - base) * rowHeight}px)`;
      paint(element, index);
    }

    this.#first = first;
    this.#last = last;
  }

  #recycle(index: number, element: HTMLElement): void {
    this.#active.delete(index);
    // Kept in the DOM but parked off-screen: removing and re-appending is the
    // expensive half of what a recycling list exists to avoid.
    element.style.transform = 'translateY(-9999px)';
    this.#pool.push(element);
  }
}
