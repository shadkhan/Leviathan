/**
 * Generate the extension's icons.
 *
 *   node scripts/icons.mjs
 *
 * Chrome needs 16, 32, 48 and 128 px PNGs, and the Web Store needs the 128.
 * They are generated rather than committed as binaries for the same reason the
 * benchmarks are scripted rather than pasted: a binary in a repository is a
 * thing nobody can review, and "why is the icon slightly wrong" is not a
 * question anyone should have to answer by opening an image editor.
 *
 * No dependencies. A PNG is a header, a handful of length-prefixed chunks and a
 * zlib stream, and `node:zlib` is already there — reaching for a raster library
 * to draw sixteen squares would be the same trade this project declines
 * everywhere else.
 */

import { deflateSync } from "node:zlib";
import { writeFileSync, mkdirSync } from "node:fs";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const out = resolve(here, "../public/icons");

/**
 * The mark, as a 16×16 grid.
 *
 * A whale, at the smallest size anything can be a whale: a body, a tail, an eye
 * and a spout. Hand-plotted because 16 px is below the size at which scaling a
 * larger drawing produces anything legible — every pixel here is a decision.
 *
 * `.` transparent · `#` body · `o` eye · `~` spout
 */
const MARK = [
  "................",
  ".............~..",
  "...........~~~..",
  "..........~...~.",
  "..#.......~~~~..",
  ".###............",
  ".####..#####....",
  ".#############..",
  ".############...",
  "..###########...",
  "...#########....",
  "....######......",
  "................",
  "................",
  "................",
  "................",
];

/** Where the eye goes, once the body is drawn. */
const EYE = { x: 4, y: 7 };

/** Deep water. Dark enough to sit on a light toolbar and a dark one. */
const BODY = [0x4a, 0x9e, 0xff, 0xff];
const SPOUT = [0x8a, 0xc4, 0xff, 0xff];
const EYE_COLOR = [0x0d, 0x11, 0x17, 0xff];
const CLEAR = [0, 0, 0, 0];

/** The colour of one cell of the mark. */
function colorAt(x, y) {
  if (x === EYE.x && y === EYE.y) {
    return EYE_COLOR;
  }
  const cell = MARK[y]?.[x] ?? ".";
  if (cell === "#") return BODY;
  if (cell === "o") return EYE_COLOR;
  if (cell === "~") return SPOUT;
  return CLEAR;
}

/**
 * Render the mark at `size` px, nearest-neighbour.
 *
 * Nearest-neighbour on purpose: the mark is pixel art, and smoothing it would
 * turn a crisp 16 px whale into a smudge at every size including 16.
 */
function render(size) {
  const scale = size / 16;
  const rgba = Buffer.alloc(size * size * 4);
  for (let y = 0; y < size; y++) {
    for (let x = 0; x < size; x++) {
      const [r, g, b, a] = colorAt(Math.floor(x / scale), Math.floor(y / scale));
      const at = (y * size + x) * 4;
      rgba[at] = r;
      rgba[at + 1] = g;
      rgba[at + 2] = b;
      rgba[at + 3] = a;
    }
  }
  return rgba;
}

/** CRC-32, as PNG defines it. */
const CRC_TABLE = (() => {
  const table = new Int32Array(256);
  for (let n = 0; n < 256; n++) {
    let c = n;
    for (let k = 0; k < 8; k++) {
      c = c & 1 ? 0xedb88320 ^ (c >>> 1) : c >>> 1;
    }
    table[n] = c;
  }
  return table;
})();

function crc32(buffer) {
  let c = 0xffffffff;
  for (const byte of buffer) {
    c = CRC_TABLE[(c ^ byte) & 0xff] ^ (c >>> 8);
  }
  return (c ^ 0xffffffff) >>> 0;
}

/** One PNG chunk: length, type, data, CRC of type+data. */
function chunk(type, data) {
  const head = Buffer.alloc(4);
  head.writeUInt32BE(data.length, 0);
  const body = Buffer.concat([Buffer.from(type, "ascii"), data]);
  const crc = Buffer.alloc(4);
  crc.writeUInt32BE(crc32(body), 0);
  return Buffer.concat([head, body, crc]);
}

/** Encode RGBA pixels as an 8-bit truecolour-with-alpha PNG. */
function png(size, rgba) {
  const header = Buffer.alloc(13);
  header.writeUInt32BE(size, 0);
  header.writeUInt32BE(size, 4);
  header[8] = 8; // bit depth
  header[9] = 6; // colour type: RGBA
  // 10, 11, 12 stay zero: deflate, adaptive filtering, no interlace.

  // Each scanline is prefixed with its filter type. Zero — "none" — because the
  // image is tiny and flat, and a filter would cost more to explain than it
  // saves in bytes.
  const raw = Buffer.alloc(size * (size * 4 + 1));
  for (let y = 0; y < size; y++) {
    const from = y * size * 4;
    raw[y * (size * 4 + 1)] = 0;
    rgba.copy(raw, y * (size * 4 + 1) + 1, from, from + size * 4);
  }

  return Buffer.concat([
    Buffer.from([0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a]),
    chunk("IHDR", header),
    chunk("IDAT", deflateSync(raw, { level: 9 })),
    chunk("IEND", Buffer.alloc(0)),
  ]);
}

mkdirSync(out, { recursive: true });

const written = [];
for (const size of [16, 32, 48, 128]) {
  const file = resolve(out, `icon-${size}.png`);
  const bytes = png(size, render(size));
  writeFileSync(file, bytes);
  written.push(`  icon-${size}.png  ${String(bytes.length).padStart(6)} B`);
}

console.log("\nleviathan icons\n");
console.log(written.join("\n"));
console.log("");
