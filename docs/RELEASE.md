# Releasing Leviathan

Four artifacts ship from one commit: a crate, a second crate, an npm package and
an extension. They share a version number because a skew between them is the
single most likely cause of a confusing bug here — the protocol asserts it at
startup rather than trusting it.

Nothing in this document is automated on purpose. Publishing is irreversible on
three of the four registries (crates.io and npm never let you reuse a version;
the Chrome Web Store queues a review you cannot cancel cleanly), and a release
script that runs unattended is a release script that eventually publishes
something nobody looked at.

---

## Before anything

```sh
pnpm check          # fmt, clippy -D warnings, all tests, wasm build, typecheck, bundle, smoke
```

Then the three gates that need a corpus and are therefore not in `check`:

```sh
# RFC 8259 — 95/95 must-accept, 185/188 must-reject with 3 documented deviations
cargo run --release -p leviathan-cli -- conformance

# RFC 9535 — 133/133 in scope, 93/93 invalid selectors refused
cargo run --release -p leviathan-cli -- cts

# Requirement 11 — token-exact and idempotent on every fixture
cargo run --release -p leviathan-cli -- fixtures ndjson  --size 2MB
cargo run --release -p leviathan-cli -- fixtures array   --size 2MB
cargo run --release -p leviathan-cli -- fixtures nested  --size 2MB
cargo run --release -p leviathan-cli -- fixtures dupkeys --size 2MB
cargo run --release -p leviathan-cli -- fixtures badutf8 --size 1MB
cargo run --release -p leviathan-cli -- roundtrip fixtures/generated/*.ndjson fixtures/generated/*.json

# And the long one, which CI does not run
cargo run --release -p leviathan-cli -- fuzz --seconds 600
```

Then re-measure anything the README claims that this release could have changed.
The numbers in the benchmark table are the credibility artifact; a stale one is
worse than no table.

```sh
cargo run --profile bench-native -p leviathan-cli -- bench fixtures/generated/ndjson-500.0MB.ndjson
```

---

## 1. `leviathan-core` → crates.io

Publish first. The other two depend on it, and crates.io will reject a crate
whose path dependency is not yet published.

```sh
cargo publish --dry-run -p leviathan-core     # packages and compiles it in isolation
cargo doc -p leviathan-core --no-deps         # must be warning-free; docs.rs uses this
cargo run --example browse  -p leviathan-core -- fixtures/generated/demo.ndjson 100
cargo run --example extract -p leviathan-core -- fixtures/generated/demo.ndjson '@.level == "error"'
```

Then, and only then:

```sh
cargo publish -p leviathan-core
```

**Check afterwards:** docs.rs builds within a few minutes. If it fails there it
fails publicly, and the version is already spent.

## 2. `leviathan-wasm` → crates.io

```sh
cargo publish --dry-run -p leviathan-wasm
cargo publish -p leviathan-wasm
```

## 3. `@shadkhan/leviathan-core` → npm

The published package is *generated*: `pnpm build:wasm` runs `wasm-pack` and
then `scripts/pack-npm.mjs`, which sets the npm name, copies both licence texts
in, and writes the `exports` map. Do not hand-edit `src/wasm/package.json` — it
is overwritten on every build.

```sh
pnpm build:wasm
cd packages/extension/src/wasm
npm pack --dry-run        # 8 files: wasm, glue, two .d.ts, two licences, README, manifest
npm publish               # `publishConfig.access: public` is already set
```

**Check afterwards:** `npm i @shadkhan/leviathan-core` in an empty directory,
then run `packages/extension/scripts/smoke.mjs` against it. That file is the
"no extension required" usage path SPEC §M7 asks for, and it is the fastest way
to find out that the package is missing a file.

## 4. The extension → Chrome Web Store

```sh
pnpm build
cd packages/extension/dist && zip -r ../leviathan-0.1.0.zip .
```

The zip must contain exactly: `manifest.json`, `background.js`, `viewer.html`,
`viewer.css`, `viewer.js`, `worker.js`, `leviathan_wasm_bg.wasm`, and
`icons/`. Anything else is a mistake — there is no build step that adds files
conditionally.

Listing copy lives in [`docs/store-listing.md`](store-listing.md). The privacy
declaration is the easy part of the submission and the reason to keep it that
way: **no data is collected**, every checkbox unchecked, and
[`PRIVACY.md`](../PRIVACY.md) as the policy URL.

**Review takes days, not minutes.** Submit before you announce anything.

## 5. Edge Add-ons

The identical zip, the same day. Edge takes the MV3 package unmodified; this is
the cheapest distribution in the plan and the only reason it is in scope.

## 6. Announce

Only after the store listing is live, because a Show HN pointing at a "pending
review" page converts once and never again.

- Tag: `git tag -a v0.1.0 -m "..."` and push.
- GitHub release, with `CHANGELOG.md`'s section as the body and the zip attached.
- Show HN. One post, and the benchmark table is the thing worth leading with.

---

## The demo, which is the actual exit criterion

SPEC §M7 does not accept "it builds" as done. It accepts a scripted demo run
start to finish without a stumble, on the 500 MB fixture:

1. Drag `ndjson-500.0MB.ndjson` onto the page. First rows appear in ~140 ms and
   the file finishes indexing in a few seconds, with the memory readout visible
   and steady.
2. Scroll to the middle. It stays smooth; the row counter keeps up.
3. Type a word into find. Matches stream in and the count rises.
4. Type `@.level == "error" && @.latency_ms > 1000`. The box widens, the tree
   filters to matching records, and the count is exact.
5. Press **Duplicates**. It reports what it finds and says how many keys it
   checked.
6. Press **Schema…**, choose a schema, watch the problems panel fill; click one
   and land on the record.
7. Choose **CSV**, press **Export…**, save. The file arrives; open it.
8. Throughout: DevTools' Network tab is empty apart from the one `.wasm` load,
   and no long task appears in the Performance panel.

Record it. The GIF of step 1 is the single most persuasive artifact this project
has, because the thing it replaces cannot do it at all.

---

## If something goes wrong after publishing

- **crates.io:** `cargo yank --version 0.1.0` stops new dependents; it does not
  remove the version and does not break existing ones. Publish `0.1.1`.
- **npm:** `npm deprecate` with a message. Unpublishing is possible within 72
  hours and is antisocial after anyone has installed it. Publish `0.1.1`.
- **Chrome Web Store:** unpublish the listing from the dashboard. Users who
  installed it keep it until the next update.

None of these are undo. That is why step 0 is a full `pnpm check` and step 1 is
a dry run.
