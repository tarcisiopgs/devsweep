#!/usr/bin/env node
// Runs the native devsweep binary shipped in the platform package that npm
// installed for this system (see optionalDependencies).
"use strict";

const { spawnSync } = require("node:child_process");

const supported = {
  "darwin-arm64": "devsweep-darwin-arm64",
  "darwin-x64": "devsweep-darwin-x64",
  "linux-arm64": "devsweep-linux-arm64",
  "linux-x64": "devsweep-linux-x64",
  "win32-arm64": "devsweep-win32-arm64",
  "win32-x64": "devsweep-win32-x64",
};
const system = `${process.platform}-${process.arch}`;
const pkg = supported[system];

if (!pkg) {
  console.error(
    `devsweep does not support ${system}. Supported: ${Object.keys(supported).join(", ")}.`
  );
  process.exit(1);
}

let binary;
try {
  const file = process.platform === "win32" ? "devsweep.exe" : "devsweep";
  binary = require.resolve(`${pkg}/bin/${file}`);
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
