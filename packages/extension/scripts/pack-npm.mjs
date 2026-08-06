/**
 * Finish the npm package that `wasm-pack` starts.
 *
 *   node scripts/pack-npm.mjs
 *
 * wasm-pack writes a correct-but-minimal `package.json`: the name comes from the
 * crate, there is no `exports` map, and the licence files it names are not in
 * the directory. None of that matters for the extension, which imports the
 * files directly — it matters for the *published* package, which SPEC §M7 makes
 * a hard requirement rather than a nice-to-have.
 *
 * This rewrites the generated manifest rather than hand-maintaining one, because
 * a hand-maintained copy drifts from the build the first time a binding is added
 * and nobody notices until a consumer does.
 */

import { copyFileSync, readFileSync, writeFileSync, existsSync } from "node:fs";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const pkg = resolve(here, "../src/wasm");
const root = resolve(here, "../../..");

const manifestPath = resolve(pkg, "package.json");
if (!existsSync(manifestPath)) {
  console.error("no package.json in src/wasm — run `pnpm build:wasm` first");
  process.exit(1);
}

const manifest = JSON.parse(readFileSync(manifestPath, "utf8"));

// The npm name is not the crate name. On crates.io this is `leviathan-wasm`,
// because that is what it is: bindings. On npm it is what a JavaScript consumer
// thinks they are installing — the engine — and the WebAssembly is an
// implementation detail they never name.
manifest.name = "@shadkhan/leviathan-core";

// Dual licence, so both texts have to travel with it. wasm-pack names the
// licence in the manifest and copies neither file.
for (const file of ["LICENSE-MIT", "LICENSE-APACHE"]) {
  copyFileSync(resolve(root, file), resolve(pkg, file));
}

manifest.files = [
  "leviathan_wasm_bg.wasm",
  "leviathan_wasm_bg.wasm.d.ts",
  "leviathan_wasm.js",
  "leviathan_wasm.d.ts",
  "LICENSE-MIT",
  "LICENSE-APACHE",
  "README.md",
];

// An `exports` map, so `import { Document } from "@shadkhan/leviathan-core"`
// resolves under Node's ESM rules and the `.wasm` stays reachable for bundlers
// that want to fingerprint it.
manifest.exports = {
  ".": {
    types: "./leviathan_wasm.d.ts",
    default: "./leviathan_wasm.js",
  },
  "./leviathan_wasm_bg.wasm": "./leviathan_wasm_bg.wasm",
  "./package.json": "./package.json",
};

// The glue is an ES module and calls `WebAssembly.instantiateStreaming`; both
// have been in Node since 16, and 18 is the oldest line still supported.
manifest.engines = { node: ">=18" };

// A scoped package is private by default, and a first publish that silently
// fails on a paid-plan check is a bad first publish.
manifest.publishConfig = { access: "public" };

// `sideEffects: ["./snippets/*"]` is wasm-pack boilerplate for a directory this
// package does not have. Left in, it tells a bundler to keep looking for
// something that is not there.
if (Array.isArray(manifest.sideEffects) && !existsSync(resolve(pkg, "snippets"))) {
  delete manifest.sideEffects;
}

writeFileSync(manifestPath, `${JSON.stringify(manifest, null, 2)}\n`);

console.log(`\n${manifest.name}@${manifest.version}\n`);
for (const file of manifest.files) {
  const at = resolve(pkg, file);
  const size = existsSync(at) ? `${readFileSync(at).length} B` : "MISSING";
  console.log(`  ${file.padEnd(30)} ${size.padStart(10)}`);
}
console.log("");

const missing = manifest.files.filter((file) => !existsSync(resolve(pkg, file)));
if (missing.length > 0) {
  console.error(`  ${missing.length} declared file(s) do not exist`);
  process.exit(1);
}
