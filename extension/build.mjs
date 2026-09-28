import { spawnSync } from "node:child_process";
import { copyFile, mkdir, readFile, rm, writeFile } from "node:fs/promises";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { APP_NAME, FULL_NAME } from "./brand.ts";

const extensionDir = dirname(fileURLToPath(import.meta.url));
const repoDir = dirname(extensionDir);
const outputDir = join(extensionDir, "dist");
const intermediateDir = join(extensionDir, ".build");

await rm(outputDir, { recursive: true, force: true });
await rm(intermediateDir, { recursive: true, force: true });
const compiler = join(repoDir, "node_modules", "typescript", "bin", "tsc");
const result = spawnSync(process.execPath, [compiler, "-p", join(extensionDir, "tsconfig.json")], {
  stdio: "inherit",
});
if (result.error) throw result.error;
if (result.status !== 0) process.exit(result.status ?? 1);

await mkdir(outputDir, { recursive: true });
for (const name of ["manifest.json", "options.html", "panel.html"]) {
  const source = await readFile(join(extensionDir, name), "utf8");
  const branded = source.replaceAll("{{FULL_NAME}}", FULL_NAME).replaceAll("{{APP_NAME}}", APP_NAME);
  if (branded.includes("{{")) throw new Error(`Unknown placeholder in extension/${name}`);
  await writeFile(join(outputDir, name), branded);
}
await copyFile(join(extensionDir, "panel.css"), join(outputDir, "panel.css"));
for (const name of [
  "brand.js", "coordination.js", "model.js", "native.js", "note-editor.js", "options.js", "panel.js", "settings.js",
  "worker.js",
]) {
  await copyFile(join(intermediateDir, "extension", name), join(outputDir, name));
}
await rm(intermediateDir, { recursive: true, force: true });
console.log(`Built unpacked Chrome extension at ${outputDir}`);
