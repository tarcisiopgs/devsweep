#!/usr/bin/env node
// Runs the native devsweep binary shipped in the platform package that npm
// installed for this Mac (see optionalDependencies).
"use strict";

const { spawnSync } = require("node:child_process");

const supported = { arm64: "@devsweep/darwin-arm64", x64: "@devsweep/darwin-x64" };
const pkg = process.platform === "darwin" ? supported[process.arch] : undefined;

if (!pkg) {
  console.error("devsweep supports macOS only (arm64, x64).");
  process.exit(1);
}

let binary;
try {
  binary = require.resolve(`${pkg}/bin/devsweep`);
} catch {
  console.error(`devsweep: the ${pkg} package is missing. Reinstall without --no-optional.`);
  process.exit(1);
}

const result = spawnSync(binary, process.argv.slice(2), { stdio: "inherit" });
if (result.error) {
  console.error(`devsweep: ${result.error.message}`);
  process.exit(1);
}
process.exit(result.status ?? 1);
