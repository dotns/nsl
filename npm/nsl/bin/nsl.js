#!/usr/bin/env node
"use strict";

const { spawnSync } = require("node:child_process");
const { existsSync } = require("node:fs");

const PLATFORM_PACKAGES = {
  "linux-x64": "@dotns/nsl-linux-x64",
  "linux-arm64": "@dotns/nsl-linux-arm64",
  "darwin-x64": "@dotns/nsl-darwin-x64",
  "darwin-arm64": "@dotns/nsl-darwin-arm64",
  "win32-x64": "@dotns/nsl-win32-x64",
};

function resolveBinary() {
  const key = `${process.platform}-${process.arch}`;
  const pkg = PLATFORM_PACKAGES[key];
  if (!pkg) {
    die(
      `unsupported platform: ${key}\n` +
        `supported: ${Object.keys(PLATFORM_PACKAGES).join(", ")}`
    );
  }

  const ext = process.platform === "win32" ? ".exe" : "";
  const subpath = `${pkg}/bin/nsl${ext}`;

  try {
    return require.resolve(subpath);
  } catch {
    die(
      `missing platform binary for ${key}\n` +
        `install failed to pull ${pkg}. Try:\n` +
        `  npm install --force ${pkg}@${require("../package.json").version}`
    );
  }
}

function die(msg) {
  process.stderr.write(`nsl: ${msg}\n`);
  process.exit(1);
}

const binary = resolveBinary();
if (!existsSync(binary)) {
  die(`binary not found at ${binary}`);
}

const result = spawnSync(binary, process.argv.slice(2), {
  stdio: "inherit",
  windowsHide: true,
});

if (result.error) {
  die(result.error.message);
}
process.exit(result.status ?? 1);
