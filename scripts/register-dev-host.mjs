#!/usr/bin/env node

// Local development registration only. Release registration belongs to the
// signed installers; this command never runs during npm install or build.
import { spawnSync } from "node:child_process";
import { randomUUID } from "node:crypto";
import { constants } from "node:fs";
import { access, mkdir, open, readFile, rename, unlink } from "node:fs/promises";
import os from "node:os";
import path from "node:path";
import { APP_NAME, NATIVE_HOST_NAME } from "../extension/brand.ts";

const HOST_NAME = NATIVE_HOST_NAME;
const DESCRIPTION = `${APP_NAME} development native host`;
const EXTENSION_ID = /^[a-p]{32}$/;

function usage() {
  throw new Error("Usage: node scripts/register-dev-host.mjs <Chrome extension ID> <absolute host binary path>");
}

const [extensionId, binaryArgument, extra] = process.argv.slice(2);
if (!extensionId || !binaryArgument || extra) usage();
if (!EXTENSION_ID.test(extensionId)) {
  throw new Error("Chrome extension ID must be 32 lowercase letters from a to p");
}
if (!path.isAbsolute(binaryArgument)) {
  throw new Error("host binary path must be absolute");
}
if (process.platform !== "darwin" && process.platform !== "win32") {
  throw new Error("development registration supports macOS and Windows only");
}

const binary = path.resolve(binaryArgument);
await access(binary, process.platform === "darwin" ? constants.X_OK : constants.F_OK);

const directory = process.platform === "darwin"
  ? path.join(os.homedir(), "Library", "Application Support", "Google", "Chrome", "NativeMessagingHosts")
  : path.join(process.env.APPDATA ?? "", APP_NAME);
if (!path.isAbsolute(directory)) {
  throw new Error("cannot determine the per-user application data directory");
}
const manifestPath = path.join(directory, `${HOST_NAME}.json`);
const manifest = {
  name: HOST_NAME,
  description: DESCRIPTION,
  path: binary,
  type: "stdio",
  allowed_origins: [`chrome-extension://${extensionId}/`],
};

await mkdir(directory, { recursive: true });
try {
  const previous = JSON.parse(await readFile(manifestPath, "utf8"));
  if (previous.description !== DESCRIPTION) {
    throw new Error(`refusing to replace a non-development host manifest at ${manifestPath}`);
  }
} catch (error) {
  if (error.code !== "ENOENT") throw error;
}

if (process.platform === "win32") {
  const registryKey = `HKCU\\Software\\Google\\Chrome\\NativeMessagingHosts\\${HOST_NAME}`;
  const existing = spawnSync("reg.exe", ["query", registryKey, "/ve"], {
    encoding: "utf8",
    windowsHide: true,
  });
  if (existing.status === 0 && !existing.stdout.toLowerCase().includes(manifestPath.toLowerCase())) {
    throw new Error(`refusing to replace an existing Chrome host registration at ${registryKey}`);
  }
}

const temporaryPath = path.join(directory, `.${HOST_NAME}.${randomUUID()}.tmp`);
const temporary = await open(temporaryPath, "wx", 0o600);
try {
  await temporary.writeFile(`${JSON.stringify(manifest, null, 2)}\n`);
  await temporary.sync();
} finally {
  await temporary.close();
}
try {
  await rename(temporaryPath, manifestPath);
} catch (error) {
  await unlink(temporaryPath);
  throw error;
}

if (process.platform === "win32") {
  const registryKey = `HKCU\\Software\\Google\\Chrome\\NativeMessagingHosts\\${HOST_NAME}`;
  const result = spawnSync("reg.exe", ["add", registryKey, "/ve", "/t", "REG_SZ", "/d", manifestPath, "/f"], {
    encoding: "utf8",
    windowsHide: true,
  });
  if (result.status !== 0) {
    throw new Error(`could not register the Chrome user-level host: ${result.stderr || result.stdout}`);
  }
}

console.log(`Registered ${HOST_NAME} for Chrome extension ${extensionId}`);
console.log(`Manifest: ${manifestPath}`);
