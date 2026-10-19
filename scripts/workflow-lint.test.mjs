import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { createHash } from "node:crypto";
import { existsSync, mkdirSync, mkdtempSync, readFileSync, readdirSync, rmSync, writeFileSync } from "node:fs";
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
