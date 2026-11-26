#!/usr/bin/env node
// Builds the `rdirstat-cli` binary for the host target and stages it as the
// Tauri sidecar `src-tauri/binaries/rdirstat-runner-<target-triple>`, the path
// `bundle.externalBin` in src-tauri/tauri.conf.json names.
//
//   node scripts/build-sidecar.mjs            debug build (tauri dev)
//   node scripts/build-sidecar.mjs --release  release build (tauri build)
//
// The app copies this binary onto a backup drive and runs it there as the
// drive's backup service (`backup serve --detach`), so it must be the CLI
// built from the same commit as the app. `beforeDevCommand` and
// `beforeBuildCommand` run this script; a plain `cargo build` does not, and
// `src-tauri/build.rs` stages a placeholder that refuses to run instead.
//
// The sidecar is named `rdirstat-runner`, not `rdirstat-cli`: Tauri copies a
// sidecar into the Cargo target directory under its bare name, which would
// overwrite the `rdirstat-cli` binary Cargo builds there.

import { execFileSync } from "node:child_process";
import fs from "node:fs";
import path from "node:path";
import process from "node:process";
import { fileURLToPath } from "node:url";

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
const release = process.argv.includes("--release");

const rustcInfo = execFileSync("rustc", ["-vV"], { cwd: root, encoding: "utf8" });
const host = /^host: (\S+)$/m.exec(rustcInfo)?.[1];
if (!host) {
  console.error("build-sidecar: cannot read the host target from `rustc -vV`");
  process.exit(1);
}
// Set by the Tauri CLI for before-commands. Only the host target is built:
// the drive's service runs on the Mac that runs the app.
const requested = process.env.TAURI_ENV_TARGET_TRIPLE;
if (requested && requested !== host) {
  console.error(
    `build-sidecar: the app is being built for ${requested}, but the sidecar is built for the host (${host}) only`,
  );
  process.exit(1);
}

const cargoArgs = ["build", "-p", "rdirstat-cli", "--bin", "rdirstat-cli"];
if (release) cargoArgs.push("--release");
execFileSync("cargo", cargoArgs, { cwd: root, stdio: "inherit" });

const targetDir = process.env.CARGO_TARGET_DIR
  ? path.resolve(root, process.env.CARGO_TARGET_DIR)
  : path.join(root, "target");
const built = path.join(targetDir, release ? "release" : "debug", "rdirstat-cli");
const staged = path.join(root, "src-tauri", "binaries", `rdirstat-runner-${host}`);

fs.mkdirSync(path.dirname(staged), { recursive: true });
const temp = `${staged}.tmp-${process.pid}`;
fs.copyFileSync(built, temp);
fs.chmodSync(temp, 0o755);
fs.renameSync(temp, staged);
console.log(`build-sidecar: ${path.relative(root, built)} -> ${path.relative(root, staged)}`);
