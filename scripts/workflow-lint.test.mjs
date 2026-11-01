import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { createHash } from "node:crypto";
import { existsSync, mkdirSync, mkdtempSync, readFileSync, readdirSync, rmSync, symlinkSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import test from "node:test";
import { install } from "./workflow-lint.mjs";

// A real zip holding fake.exe, built with System32's bsdtar, the tar install() unpacks with.
const tar = join(process.env.SystemRoot ?? "C:\\Windows", "System32", "tar.exe");
function fixture() {
  const work = mkdtempSync(join(tmpdir(), "wl-test-"));
  writeFileSync(join(work, "fake.exe"), "not really an exe");
  const made = spawnSync(tar, ["-a", "-cf", "tool.zip", "fake.exe"], { cwd: work });
  assert.equal(made.status, 0);
  const bytes = readFileSync(join(work, "tool.zip"));
  const cache = join(work, "cache");
  mkdirSync(cache);
  const spec = { version: "1.0.0", url: "https://example.invalid/tool.zip", sha256: createHash("sha256").update(bytes).digest("hex") };
  return { work, cache, bytes, spec };
}

test("install rejects a download whose SHA-256 differs and caches nothing", async () => {
  const { work, cache, spec } = fixture();
  try {
    await assert.rejects(install("fake", spec, { cache, download: async () => Buffer.from("tampered") }), /failed SHA-256/);
    assert.deepEqual(readdirSync(cache), []);
  } finally { rmSync(work, { recursive: true, force: true }); }
});

test("install downloads, verifies and unpacks the exe, then reuses it without downloading", async () => {
  const { work, cache, bytes, spec } = fixture();
  try {
    const exe = await install("fake", spec, { cache, download: async () => bytes });
    assert.ok(existsSync(exe));
    const again = await install("fake", spec, { cache, download: async () => assert.fail("downloaded again") });
    assert.equal(again, exe);
  } finally { rmSync(work, { recursive: true, force: true }); }
});

test("install tolerates another slot winning the rename race", async () => {
  const { work, cache, bytes, spec } = fixture();
  try {
    const download = async () => {
      // The other slot installs while this one downloads.
      const dir = join(cache, "fake-1.0.0-win32_x64");
      mkdirSync(dir);
      writeFileSync(join(dir, "fake.exe"), "the other slot's copy");
      return bytes;
    };
    const exe = await install("fake", spec, { cache, download });
    assert.equal(readFileSync(exe, "utf8"), "the other slot's copy");
    assert.deepEqual(readdirSync(cache), ["fake-1.0.0-win32_x64"]);
  } finally { rmSync(work, { recursive: true, force: true }); }
});

test("install rethrows a failed rename when no other slot's exe is in place", async () => {
  const { work, cache, bytes, spec } = fixture();
  try {
    const download = async () => {
      // A slot folder without the exe blocks the rename, and nothing usable is cached.
      const dir = join(cache, "fake-1.0.0-win32_x64");
      mkdirSync(dir);
      writeFileSync(join(dir, "other.txt"), "not the exe");
      return bytes;
    };
    await assert.rejects(install("fake", spec, { cache, download }), { syscall: "rename" });
  } finally { rmSync(work, { recursive: true, force: true }); }
});

for (const name of ["workflow-lint", "complexity", "cognitive"]) {
  test(`${name} run through a junction still runs its checks`, (t) => {
    const work = mkdtempSync(join(tmpdir(), "wl-test-"));
    const link = join(work, "scripts");
    // Registered before the junction is made, so a failed junction still removes the temp folder.
    // Unlink the junction first, so the cleanup can never reach the real scripts.
    t.after(() => { rmSync(link, { force: true }); rmSync(work, { recursive: true, force: true }); });
    symlinkSync(import.meta.dirname, link, "junction");
    // Stop at the first external operation: reaching it shows main ran, without downloads or Cargo.
    const source = `import cp from 'node:child_process';
      import { syncBuiltinESMExports } from 'node:module';
      cp.spawnSync = globalThis.fetch = () => { throw new Error('checks stubbed'); };
      syncBuiltinESMExports();`;
    const stub = `data:text/javascript,${encodeURIComponent(source)}`;
    const env = { ...process.env, SPIDER_PINNED_CACHE: join(work, "cache") };
    const ran = spawnSync(process.execPath, ["--import", stub, join(link, `${name}.mjs`)], { env, encoding: "utf8" });
    assert.equal(ran.status, 1);
    assert.match(ran.stderr, /checks stubbed/);
  });
}

test("imported with no script path, as from node -e, it runs nothing and does not throw", () => {
  const script = JSON.stringify(new URL("./workflow-lint.mjs", import.meta.url).href);
  const ran = spawnSync(process.execPath, ["--input-type=module", "-e", `await import(${script})`], { encoding: "utf8" });
  assert.equal(ran.status, 0, ran.stderr);
});

for (const name of ["workflow-lint", "complexity", "cognitive"]) {
  for (const mode of ["eval", "stdin"]) {
    test(`${name} imported from ${mode} with a non-file script path runs nothing`, () => {
      const source = `await import(${JSON.stringify(new URL(`./${name}.mjs`, import.meta.url).href)})`;
      const args = mode === "eval" ? ["-e", source, "silk-no-such-script"] : ["-"];
      const ran = spawnSync(process.execPath, ["--input-type=module", ...args], { input: source, encoding: "utf8" });
      assert.equal(ran.status, 0, ran.stderr);
      assert.equal(ran.stdout, "");
      assert.equal(ran.stderr, "");
    });
  }
}

// Every ci.yml job sends a fork's pull request to a GitHub-hosted runner, never a self-hosted
// SPIDER_* one (SILK-S82-T1); actionlint and zizmor accept a flipped or dropped guard (SILK-S85-T1).
const forkGuard = /^\$\{\{ github\.event_name == 'pull_request' && github\.event\.pull_request\.head\.repo\.full_name != github\.repository && '((?:windows|ubuntu|macos)-(?:latest|\d[\d.]*))' \|\| vars\.SPIDER_\w+_RUNNER \|\| '\1' \}\}$/;
function unguardedJobs(yaml) {
  const jobs = new Map();
  let job;
  for (const line of yaml.slice(yaml.search(/^jobs:/m)).split("\n")) {
    // Any key at job depth is a job, quoted or flow-style too, so one this can't read is flagged.
    const start = /^ {2}([^\s#][^:]*)/.exec(line);
    if (start) jobs.set(job = start[1], undefined);
    const runsOn = /^ {4}runs-on:\s*(.*?)\s*$/.exec(line);
    if (runsOn && job) jobs.set(job, runsOn[1]);
  }
  assert.ok(jobs.size > 0, "no jobs found in ci.yml");
  return [...jobs].filter(([, runsOn]) => !forkGuard.test(runsOn ?? "")).map(([name]) => name);
}
const ciYml = readFileSync(new URL("../.github/workflows/ci.yml", import.meta.url), "utf8").replaceAll("\r\n", "\n");

test("every ci.yml job keeps fork pull requests on a GitHub-hosted runner", () => {
  assert.deepEqual(unguardedJobs(ciYml), []);
});

test("the fork guard check catches a flipped comparison, a dropped guard and a self-hosted fork label", () => {
  const flipped = ciYml.replace("full_name != github.repository", "full_name == github.repository");
  assert.deepEqual(unguardedJobs(flipped), ["windows"]);
  const dropped = ciYml.replace(/(\n {2}deny:\n {4}runs-on: )[^\n]*/, "$1${{ vars.SPIDER_WINDOWS_RUNNER || 'windows-latest' }}");
  assert.deepEqual(unguardedJobs(dropped), ["deny"]);
  const selfHosted = ciYml.replace("&& 'ubuntu-latest' ||", "&& vars.SPIDER_LINUX_RUNNER ||");
  assert.deepEqual(unguardedJobs(selfHosted), ["workflows"]);
  const added = ciYml.replace(/\njobs:\n/, "\njobs:\n  extra:\n    runs-on: self-hosted\n    steps: []\n");
  assert.deepEqual(unguardedJobs(added), ["extra"]);
  const noRunsOn = ciYml.replace(/\njobs:\n/, "\njobs:\n  reused: # a reusable workflow\n    uses: ./.github/workflows/other.yml\n");
  assert.deepEqual(unguardedJobs(noRunsOn), ["reused"]);
});

test("the fork guard check fails closed on a flow-style or quoted job key", () => {
  const flow = `${ciYml.trimEnd()}\n  extra: { runs-on: self-hosted, steps: [] }\n`;
  assert.deepEqual(unguardedJobs(flow), ["extra"]);
  // A quoted job after deny must not hide deny's own self-hosted runs-on.
  const guarded = /\n {2}deny:\n {4}runs-on: ([^\n]*)/.exec(ciYml)[1];
  const quoted = ciYml.replace(/(\n {2}deny:\n {4}runs-on: )[^\n]*/, `$1self-hosted`)
    .replace(/\n {2}# The workflows/, `\n  "extra":\n    runs-on: ${guarded}\n    steps: []\n  # The workflows`);
  assert.deepEqual(unguardedJobs(quoted), ["deny"]);
});

test("the fork guard check rejects a self-hosted label that looks GitHub-hosted", () => {
  const lookalike = ciYml.replaceAll("'windows-latest'", "'windows-selfhosted'");
  assert.deepEqual(unguardedJobs(lookalike), ["windows", "deny"]);
});
