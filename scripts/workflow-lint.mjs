// CI's `workflows` job for the gate: actionlint and zizmor at the versions CI pins (ci.yml). Windows x64
// release zips are SHA-256 pinned in workflow-lint/tools.json and cached per version, so a node needs
import { spawnSync } from "node:child_process";
import { createHash } from "node:crypto";
import { existsSync, mkdirSync, mkdtempSync, readFileSync, renameSync, rmSync, writeFileSync } from "node:fs";
import { homedir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const root = dirname(dirname(fileURLToPath(import.meta.url)));
const tools = JSON.parse(readFileSync(new URL("./workflow-lint/tools.json", import.meta.url), "utf8"));
const cache = process.env.SPIDER_PINNED_CACHE ?? join(homedir(), ".cache", "spider");
const sha256 = bytes => createHash("sha256").update(bytes).digest("hex");

function run(exe, args) {
  const result = spawnSync(exe, args, { stdio: "inherit", cwd: root });
  if (result.error) throw result.error;
  return result.status ?? 1;
}

async function install(name, { version, url, sha256: expected }) {
  const dir = join(cache, `${name}-${version}-win32_x64`);
  const exe = join(dir, `${name}.exe`);
  if (existsSync(exe)) return exe;
  const response = await fetch(url, { signal: AbortSignal.timeout(120_000) });
  if (!response.ok) throw new Error(`Downloading ${url}: HTTP ${response.status}`);
  const bytes = Buffer.from(await response.arrayBuffer());
  if (sha256(bytes) !== expected) throw new Error(`${url} failed SHA-256 verification`);
  mkdirSync(dirname(dir), { recursive: true });
  const tmp = mkdtempSync(`${dir}-`);
  try {
    writeFileSync(join(tmp, "tool.zip"), bytes);
    // System32's bsdtar reads zip; Git for Windows' GNU tar does not.
    const tar = join(process.env.SystemRoot ?? "C:\\Windows", "System32", "tar.exe");
    const unpacked = spawnSync(tar, ["-xf", "tool.zip", `${name}.exe`], { cwd: tmp, encoding: "utf8" });
    if (unpacked.status !== 0) throw new Error(`Unpacking ${name}: ${unpacked.error?.message ?? unpacked.stderr}`);
    // Another slot may have won the race; its copy is checksum-verified the same way.
    try { renameSync(tmp, dir); } catch (error) { if (!existsSync(exe)) throw error; }
  } finally {
    rmSync(tmp, { recursive: true, force: true });
  }
  return exe;
}

async function main() {
  const actionlint = await install("actionlint", tools.actionlint);
  const zizmor = await install("zizmor", tools.zizmor);
  const syntax = run(actionlint, ["-color"]);
  const security = run(zizmor, ["--offline", ".github"]);
  return syntax || security;
}

try { process.exitCode = await main(); } catch (error) { console.error(error.message); process.exitCode = 1; }
