/**
 * Parsing a path back into steps — the inverse of what "Copy path" produces.
 *
 * A viewer that hands out `$.orders[3].id` and cannot take it back is half a
 * tool: the path is the thing users paste into tickets, chat and each other's
 * terminals, and it should come home again.
 *
 * DOM-free and engine-free, because string parsing with quotes, escapes and
 * bracket forms is exactly the kind of code that is either right for every
 * input or subtly wrong for a few — and "subtly wrong for a few" here means
 * silently navigating to the wrong record (C45, C52).
 */

/** One step of a parsed path: an array index, or an object key. */
export type Step = { index: number } | { key: string };

/** Characters that end an unquoted segment. */
const TERMINATORS = new Set([".", "[", "]"]);

/**
 * Parse `$.orders[3]["odd key"].id` into steps.
 *
 * Deliberately more permissive than the generator is strict: the leading `$` is
 * optional and a bare `orders[3]` works, because a path that has been through a
 * chat client or a shell often arrives slightly chewed. What it will not do is
 * guess — anything it cannot read returns `undefined` so the caller can try
 * reading it as a number instead.
 */
export function parsePath(text: string): Step[] | undefined {
  const source = text.trim().replace(/^\$/, "");
  const steps: Step[] = [];
  let at = 0;

  while (at < source.length) {
    const char = source[at];

    if (char === ".") {
      at++;
      const start = at;
      while (at < source.length && !TERMINATORS.has(source[at] as string)) {
        at++;
      }
      if (at === start || source[at] === "]") {
        // `..`, a trailing dot, or a stray `]`. The generator only ever emits a
        // dotted segment for an identifier-shaped key — anything else goes in
        // brackets — so a `]` here means the path is damaged, and guessing at a
        // damaged path navigates somewhere confidently wrong.
        return undefined;
      }
      steps.push({ key: source.slice(start, at) });
      continue;
    }

    if (char === "[") {
      const close = source.indexOf("]", at);
      if (close < 0) {
        return undefined;
      }
      const inner = source.slice(at + 1, close).trim();
      at = close + 1;

      if (/^\d+$/.test(inner)) {
        steps.push({ index: Number(inner) });
        continue;
      }
      const quoted = readQuoted(inner);
      if (quoted === undefined) {
        return undefined;
      }
      steps.push({ key: quoted });
      continue;
    }

    // A bare first segment: `orders[3]` rather than `.orders[3]`.
    if (steps.length === 0) {
      const start = at;
      while (at < source.length && !TERMINATORS.has(source[at] as string)) {
        at++;
      }
      if (at === start || source[at] === "]") {
        return undefined;
      }
      steps.push({ key: source.slice(start, at) });
      continue;
    }
    return undefined;
  }

  return steps.length > 0 ? steps : undefined;
}

/**
 * Read `"a key"` or `'a key'`, honouring backslash escapes.
 *
 * Hand-rolled rather than `JSON.parse`, which would reject the single-quoted
 * form that people type and would need the string re-quoted first — and
 * re-quoting a string that may contain quotes is how an escaping bug is born.
 */
function readQuoted(inner: string): string | undefined {
  const quote = inner[0];
  if (
    (quote !== '"' && quote !== "'") ||
    inner.length < 2 ||
    inner.at(-1) !== quote
  ) {
    return undefined;
  }

  const body = inner.slice(1, -1);
  let out = "";
  for (let at = 0; at < body.length; at++) {
    if (body[at] !== "\\") {
      out += body[at];
      continue;
    }
    at++;
    const escaped = body[at];
    if (escaped === undefined) {
      return undefined; // a trailing backslash
    }
    out += escaped === "n" ? "\n" : escaped === "t" ? "\t" : escaped;
  }
  return out;
}
