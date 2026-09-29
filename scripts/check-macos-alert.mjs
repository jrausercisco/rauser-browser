#!/usr/bin/env node

// Real-alert check for the macOS Yes/No confirmation: shows the real alert and
// checks that it closes by itself, with no click, when the host closes the
// dialog child's stdin (the cancel path) and when it times out. The alert is
// drawn by UserNotificationCenter, so the check counts that process's
// on-screen windows. It puts alerts on the user's screen, so it runs only when
// given --on-desktop. See 2026-09-28 TESTING.md, "Real-UI acceptance".

import { spawn } from "node:child_process";
import { mkdtemp, rm } from "node:fs/promises";
import os from "node:os";
import path from "node:path";
import { setTimeout as delay } from "node:timers/promises";
import { fileURLToPath } from "node:url";
import { BINARY_NAME } from "../extension/brand.ts";

const REPO = path.dirname(path.dirname(fileURLToPath(import.meta.url)));
const DEFAULT_HOST = path.join(REPO, "target", "debug", BINARY_NAME);
const SHOW_ALERT = path.join(REPO, "target", "debug", "examples", "show_alert");
const BUILD = `cargo build --locked -p ${BINARY_NAME} && cargo build --locked -p brauser-macos-alert --example show_alert`;
const CONFIRM_COMMAND = "__dialog-confirm";
// How long an alert may take to appear, and to go once it should close.
const APPEAR_LIMIT_MS = 5000;
const CLOSE_LIMIT_MS = 1500;
const TIMEOUT_SECONDS = 1;

function usage() {
  console.log(`Usage: node scripts/check-macos-alert.mjs [--host /absolute/path/to/${BINARY_NAME}] [--on-desktop]`);
  console.log("  --on-desktop  Required. Shows two alerts on screen for about 3 seconds each; do not click them.");
  console.log(`Build first with:\n  ${BUILD}`);
}

async function commandOutput(executable, args) {
  return new Promise((resolve, reject) => {
    const child = spawn(executable, args, { stdio: ["ignore", "pipe", "pipe"] });
    let stdout = "";
    let stderr = "";
    child.stdout.setEncoding("utf8");
    child.stderr.setEncoding("utf8");
    child.stdout.on("data", (chunk) => { stdout += chunk; });
    child.stderr.on("data", (chunk) => { stderr += chunk; });
    child.on("error", reject);
    child.on("close", (code) => {
      if (code === 0) resolve(stdout.trim());
      else reject(new Error(`${executable} exited ${code}: ${stderr.trim()}`));
    });
  });
}

async function alertWindows(windowCheck) {
  const pids = (await commandOutput("/usr/bin/pgrep", ["-x", "UserNotificationCenter"]).catch(() => "")).split(/\s+/).filter(Boolean);
  if (pids.length === 0) return 0;
  return JSON.parse(await commandOutput(windowCheck, pids)).windows;
}

async function waitForWindows(windowCheck, wanted, limitMs) {
  const start = Date.now();
  for (;;) {
    const count = await alertWindows(windowCheck);
    if (wanted(count)) return Date.now() - start;
    if (Date.now() - start > limitMs) throw new Error(`alert windows stayed at ${count} for ${limitMs} ms`);
    await delay(100);
  }
}

function exited(child) {
  return new Promise((resolve, reject) => {
    child.on("error", reject);
    child.on("close", (code) => resolve(code));
  });
}

function collect(stream) {
  let text = "";
  stream.setEncoding("utf8");
  stream.on("data", (chunk) => { text += chunk; });
  return () => text.trim();
}

// The host's own dialog child, closed the way the host closes it when the
// extension goes away: its stdin ends.
async function checkCancel(host, windowCheck) {
  const child = spawn(host, [CONFIRM_COMMAND], { stdio: ["pipe", "pipe", "pipe"] });
  const stderr = collect(child.stderr);
  const done = exited(child);
  const request = "Brauser alert check\nThis closes by itself. Please do not click.";
  try {
    child.stdin.write(`${request.length}\n${request}`);
    await waitForWindows(windowCheck, (count) => count > 0, APPEAR_LIMIT_MS);
    await delay(1000);
    child.stdin.end();
    const closedMs = await waitForWindows(windowCheck, (count) => count === 0, CLOSE_LIMIT_MS);
    const code = await done;
    if (code === 0) throw new Error("the dialog child reported an answer instead of being canceled");
    return `closed ${closedMs} ms after stdin ended; child exited ${code}: ${stderr()}`;
  } finally {
    child.kill();
  }
}

async function checkTimeout(windowCheck) {
  const child = spawn(SHOW_ALERT, [String(TIMEOUT_SECONDS)], { stdio: ["ignore", "pipe", "pipe"] });
  const stdout = collect(child.stdout);
  const done = exited(child);
  try {
    const shownMs = await waitForWindows(windowCheck, (count) => count > 0, APPEAR_LIMIT_MS);
    const goneMs = await waitForWindows(windowCheck, (count) => count === 0, TIMEOUT_SECONDS * 1000 + CLOSE_LIMIT_MS);
    const code = await done;
    if (code !== 0 || stdout() !== "declined") throw new Error(`a timed-out alert returned ${JSON.stringify(stdout())} (exit ${code})`);
    return `shown after ${shownMs} ms, closed ${goneMs} ms later, answered "declined"`;
  } finally {
    child.kill();
  }
}

async function main() {
  const args = process.argv.slice(2);
  let host = DEFAULT_HOST;
  let onDesktop = false;
  for (let index = 0; index < args.length; index += 1) {
    if (args[index] === "--help") return usage();
    if (args[index] === "--on-desktop") onDesktop = true;
    else if (args[index] === "--host" && path.isAbsolute(args[index + 1] ?? "")) host = args[++index];
    else throw new Error(`unknown argument ${args[index]}; run with --help`);
  }
  if (process.platform !== "darwin") throw new Error("this check runs only on macOS");
  if (!onDesktop) {
    throw new Error("this check shows real alerts on screen; run it with --on-desktop, and only when the user has agreed");
  }
  const runDir = await mkdtemp(path.join(os.tmpdir(), "brauser-alert-check-"));
  try {
    const windowCheck = path.join(runDir, "smoke-macos-windows");
    await commandOutput("/usr/bin/swiftc", ["-O", path.join(REPO, "scripts", "smoke-macos-windows.swift"), "-o", windowCheck]);
    if ((await alertWindows(windowCheck)) !== 0) throw new Error("an alert is already on screen; close it and rerun");
    console.log(`PASS: cancel path: ${await checkCancel(host, windowCheck)}`);
    console.log(`PASS: timeout path: ${await checkTimeout(windowCheck)}`);
  } finally {
    await rm(runDir, { recursive: true, force: true });
  }
}

main().catch((error) => {
  console.error(`FAIL: ${error.message}`);
  process.exitCode = 1;
});
