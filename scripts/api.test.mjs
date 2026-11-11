// api.js against a fake Tauri: the theme side of `talk` and `openUrl` (SILK-S30-T1). The event names and
// payloads are the ones stub/src/main.rs's emit_output and emit_exit test pins.
import test from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
// The silk.yml gate path tests ride along: a silk.yml change selects only the tests entry, which runs this file.
import "./gate-paths.test.mjs";

function load() {
  const listeners = {};
  const calls = [];
  const window = {
    __TAURI__: {
      event: { listen: async (name, fn) => void (listeners[name] = fn) },
      core: { invoke: async (cmd, args) => void calls.push([cmd, args]) },
    },
  };
  const src = readFileSync(new URL("../stub/src/api.js", import.meta.url), "utf8").replace("/*INFO*/", "{}");
  new Function("window", "document", src)(window, { addEventListener() {} });
  return { silk: window.silk, listeners, calls };
}

test("a talk program's lines reach on('output') in order and its exit code reaches on('exit')", async () => {
  const { silk, listeners } = load();
  const seen = [];
  silk.on("output", (line) => seen.push(line));
  silk.on("exit", (code) => seen.push(`exit ${code}`));
  await silk.start();
  for (const line of ["name?", "hello Ann"]) listeners["silk:output"]({ payload: line });
  listeners["silk:exit"]({ payload: 7 });
  assert.deepEqual(seen, ["name?", "hello Ann", "exit 7"]);
});

test("silk.write and silk.openUrl invoke the write and open_url commands with their argument unchanged", async () => {
  const { silk, calls } = load();
  const url = "HTTPS://example.com/sign in?code=a,b&x=%20#top";
  await silk.write("Ann");
  await silk.openUrl(url);
  assert.deepEqual(calls, [["write", { line: "Ann" }], ["open_url", { url }]]);
});
