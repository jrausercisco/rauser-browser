import { spawnSync } from "node:child_process";
import { copyFile, mkdir, rm } from "node:fs/promises";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

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
for (const name of ["manifest.json", "panel.html", "panel.css"]) {
  await copyFile(join(extensionDir, name), join(outputDir, name));
}
for (const name of ["model.js", "native.js", "panel.js", "worker.js"]) {
  await copyFile(join(intermediateDir, "extension", name), join(outputDir, name));
}
await rm(intermediateDir, { recursive: true, force: true });
console.log(`Built unpacked Chrome extension at ${outputDir}`);
