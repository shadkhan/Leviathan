# Privacy policy

**Leviathan collects nothing, sends nothing, and stores nothing.**

That is the whole policy. The rest of this page exists so you do not have to
take it on trust.

## What the extension does with your data

Your file is opened by the browser's own file picker or dropped onto the page.
From there it is read in byte ranges by a Web Worker running on your machine and
indexed in WebAssembly memory that belongs to that tab. When you close the tab,
it is gone.

No copy is made. Nothing is uploaded. Nothing is written to disk unless you
press **Export…**, and then it goes exactly where the browser's save dialog puts
it.

| | |
|---|---|
| Analytics, telemetry, crash reporting | None. There is no reporting code. |
| Accounts, sign-in, sync | None. There is nothing to sign in to. |
| Cookies, local storage, IndexedDB | Not used. |
| Remote code, CDNs, fonts | None. The `.wasm` is bundled in the package. |
| Network requests | One, to the extension's own package. See below. |

## How to check this yourself, in about a minute

The strongest thing this policy can offer is that you do not need to believe it.

**1. Read the manifest.** Install the extension, open
`chrome://extensions`, enable Developer mode, and look at the permissions. Or
read [`manifest.json`](packages/extension/public/manifest.json) in the source:

```json
"permissions": [],
"host_permissions": [],
```

Both are empty. A Chrome extension **cannot** make a cross-origin request
without a host permission, and Chrome enforces that, not us. An extension that
wanted to phone home would have to declare where — in a file the store shows you
before you install it.

**2. Read the content security policy.** In the same file:

```json
"script-src 'self' 'wasm-unsafe-eval'; object-src 'self'; worker-src 'self'"
```

`'self'` means code loaded from the extension package and nowhere else. No CDN,
no remote script, no eval of downloaded code.

**3. Watch the network.** Open DevTools, switch to the Network tab, and drop a
file in. Load it, browse it, search it, export it.

You will see **exactly one request**, and it is worth being precise about it
rather than claiming zero: the Worker loads `leviathan_wasm_bg.wasm` from
`chrome-extension://<id>/`, which is the engine itself, shipped inside the
package you installed. It happens once, at startup, before your file is touched.
There is no second request, and there is no request after it for the life of the
tab.

**4. Read the source.** It is [open](https://github.com/shadkhan/leviathan),
and it is not large. `XMLHttpRequest`, `WebSocket` and `sendBeacon` appear
nowhere in it. The single `fetch` is the one described above, in the WebAssembly
loader; you can find it by searching for `WASM_URL`. That is a much easier thing
to verify than network code that claims to be well behaved.

## Why it is built this way

The people this tool is for paste production data into it: API responses with
customer records, log exports, database dumps. For them, "we anonymize your
telemetry" is not reassuring — it means there is a pipeline, and a pipeline can
be misconfigured.

Requesting zero permissions costs a feature. Leviathan cannot open a URL you
paste, because fetching one needs a host permission, and that permission would
make every sentence above unverifiable. Downloading the file yourself and
dropping it in is a smaller inconvenience than the alternative, so that is the
trade this makes.

## Changes

Any change to this policy is a change to the manifest, and a change to the
manifest is visible in the store listing, in the extension's permissions screen,
and in the repository's history. If a future version ever requests a permission,
it will say so here and in [`CHANGELOG.md`](CHANGELOG.md) first.

## Contact

Questions or a security report: open an issue at
<https://github.com/shadkhan/leviathan/issues>.

*Last updated: 2026-08-05. Applies to version 0.1.0 onward.*
