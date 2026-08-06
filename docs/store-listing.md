# Store listing copy

Everything the Chrome Web Store and Edge Add-ons submission forms ask for, kept
here so the wording is reviewed in a pull request rather than typed into a
browser form at midnight.

The audience is a backend, data or platform engineer who just watched a tab die.
The listing is written for the moment *after* that, which is why it opens with
the failure rather than with the feature list.

---

## Name

```
Leviathan — large JSON viewer
```

## Short description (132 characters max)

```
Open multi-gigabyte JSON and NDJSON without freezing your browser. Tested to 8 GB. Streams locally: no upload, no permissions.
```

*(122 characters.)*

## Category

Developer Tools

## Detailed description

```
Every JSON viewer in your browser does the same thing: JSON.parse(await file.text()).

That holds your file as a UTF-16 string at twice its size, then builds an object graph three to ten times larger again. A 500 MB file asks for several gigabytes on the main thread, and the tab dies.

Leviathan never parses the file into a value. It indexes it.

A Rust engine compiled to WebAssembly streams the file in a Web Worker and records only where each node starts — 8 bytes each, 14 MB of index for a 500 MB file. Key names and values are re-read from the file at the moment a row is painted: microseconds each, instead of gigabytes to store them.

MEASURED, NOT CLAIMED

• 8 GB NDJSON: 28.2 million records indexed in 17.8 seconds, using 539 MB
• Fifty rows from record 28,239,470: 2.5 milliseconds
• 500 MB: first rows painted in 141 ms, indexed in 3.6-6.8 seconds, 22 MB of memory
• Scrolling 100,000 rows: median 16.6 ms per frame, zero long tasks
• A filter across 1.77 million records: 8.7 seconds, interactive throughout

Every figure comes from running the shipped engine against a generated fixture. The full table, including the one criterion that is missed and the file shape that does not fit, is in the README.

WHAT IT DOES

• View — virtualized tree, breadcrumb, full keyboard navigation, dark mode
• Find — literal search across the whole file, streamed, not just what is on screen
• Filter — @.status == "error" && @.latency_ms > 1000, evaluated per record
• Validate — byte, line and column accurate errors you can jump to, plus JSON Schema
• Duplicates — repeated object keys, which are valid JSON that every parser resolves differently and nothing else warns you about
• Export — JSON, NDJSON or CSV, streamed to disk, byte-faithful

It survives broken files too. A truncated dump, one bad escape at 90% depth, a log rotation mid-record: every stage degrades instead of aborting. "It won't open" is the failure mode of the tools this replaces.

ZERO PERMISSIONS

Check the permissions list above: it is empty. Not "minimal" — empty. A Chrome extension cannot make a cross-origin request without a host permission, and this one declares none, so "your data never leaves your machine" is something you can verify in about ten seconds rather than something you have to believe.

That costs a feature: Leviathan cannot fetch a URL you paste, because fetching one would need exactly the permission that makes the sentence above checkable. Download the file and drop it in.

OPEN SOURCE

MIT OR Apache-2.0. The engine is published separately on crates.io and npm, so you can use it without the extension.

https://github.com/shadkhan/leviathan
```

## Screenshots (1280×800 or 640×400, up to 5)

Numbered in the order they should appear. Each has a job; a screenshot that only
shows that the software has a user interface is a wasted slot.

1. **A 500 MB file, open and browsable.** The tree mid-scroll, the file name and
   size in the header, the memory readout visible. This is the whole pitch.
2. **The filter in use.** `@.level == "error" && @.latency_ms > 1000` in the box,
   the tree filtered, the result count showing.
3. **The problems panel after a schema check**, with a validation error selected
   and the tree scrolled to the offending record.
4. **Duplicate keys reported**, with both locations visible.
5. **The permissions screen from `chrome://extensions`**, showing the empty
   list. Unusual for a screenshot slot, and the most persuasive one for the
   audience this is for.

## Promotional tile (440×280)

The whale mark on `#0d1117`, with "A JSON viewer that survives large files"
beneath it. No screenshot in the tile — at 440×280 a screenshot of a tree view
is unreadable noise.

## Privacy practices declaration

| Field | Answer |
|---|---|
| Does the item collect user data? | **No** |
| Personally identifiable information | Not collected |
| Health, financial, authentication information | Not collected |
| Personal communications, location, web history | Not collected |
| User activity, website content | Not collected |
| Remote code | **No** — the `.wasm` is bundled in the package |
| Privacy policy URL | `https://github.com/shadkhan/leviathan/blob/main/PRIVACY.md` |

Single purpose statement:

```
Leviathan opens JSON and NDJSON files from the user's own machine and displays, searches, validates and exports them locally.
```

Justification for `wasm-unsafe-eval` in the content security policy — the one
thing a reviewer will ask about:

```
The extension's parsing engine is compiled to WebAssembly and bundled inside the extension package. Instantiating a bundled .wasm module requires 'wasm-unsafe-eval' in the extension_pages CSP. No remote code is loaded: script-src is 'self' only, and the manifest requests no host permissions.
```

## Support and links

| | |
|---|---|
| Homepage | `https://github.com/shadkhan/leviathan` |
| Support | `https://github.com/shadkhan/leviathan/issues` |
| Privacy policy | `https://github.com/shadkhan/leviathan/blob/main/PRIVACY.md` |

## Edge Add-ons

The same package, the same copy. Edge's form asks for a short description under
200 characters and does not have a promotional tile; everything else maps
one to one.
