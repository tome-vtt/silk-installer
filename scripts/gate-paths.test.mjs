// Which silk.yml gate entries a changed path selects (SILK-S86-T1), so a nested source still runs the checks and
// docs/ runs none (SILK-S64-T2). api.test.mjs imports this file: the gate's tests entry and CI run only that one.
import test from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { posix } from "node:path";

// Mirrors covers() in Spider's plugin/scripts/gate/gate-entry.mjs, which this repository does not vendor.
const covers = (entry, p) => {
  if (!entry.paths?.length) return true;
  const excluded = entry.paths.some((g) => g.startsWith("!") && posix.matchesGlob(p, g.slice(1)));
  return !excluded && entry.paths.some((g) => !g.startsWith("!") && posix.matchesGlob(p, g));
};

const unquote = (s) => s.trim().replace(/^(["'])(.*)\1$/, "$2");

// ponytail: reads only the shapes silk.yml's gate uses (`- name:`, inline or block `paths:`); a YAML parser if it grows.
function gateEntries(yml) {
  const gate = yml.split(/^gate:\r?\n/m)[1].split(/^\S/m)[0];
  const entries = [];
  let block = null;
  for (const line of gate.split(/\r?\n/)) {
    const name = line.match(/^ {2}- name: (.+)$/);
    const inline = line.match(/^ {4}paths: \[(.*)\]$/);
    const item = line.match(/^ {6}- (.+)$/);
    if (name) entries.push({ name: name[1].trim(), paths: [] });
    else if (inline) entries.at(-1).paths.push(...inline[1].split(",").map(unquote));
    else if (/^ {4}paths:\s*$/.test(line)) block = entries.at(-1).paths;
    else if (item && block) block.push(unquote(item[1]));
    else block = null;
  }
  return entries;
}

const entries = gateEntries(readFileSync(new URL("../silk.yml", import.meta.url), "utf8"));
const selects = (p) => entries.filter((e) => covers(e, p)).map((e) => e.name);

test("a nested Rust source selects tests, fmt, complexity, clippy and cognitive", () => {
  assert.deepEqual(selects("stub/src/install.rs"), ["tests", "fmt", "complexity", "clippy", "cognitive"]);
});

test("a crate's Cargo.toml selects tests, complexity, clippy and deny", () => {
  assert.deepEqual(selects("compiler/Cargo.toml"), ["tests", "complexity", "clippy", "deny"]);
});

test("the root config files keep their entries", () => {
  assert.deepEqual(selects("Cargo.toml"), ["tests", "complexity", "clippy", "deny"]);
  assert.deepEqual(selects("Cargo.lock"), ["tests", "clippy", "deny"]);
  assert.deepEqual(selects("rustfmt.toml"), ["tests", "fmt"]);
  assert.deepEqual(selects("clippy.toml"), ["tests", "complexity", "clippy"]);
  assert.deepEqual(selects("deny.toml"), ["tests", "deny"]);
});

test("docs/ selects no entry", () => {
  assert.deepEqual(selects("docs/x.rs"), []);
  assert.deepEqual(selects("docs/a/Cargo.toml"), []);
});
