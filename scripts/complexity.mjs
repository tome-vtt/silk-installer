// Holds clippy's cognitive_complexity offenders to complexity-baseline.json, which only shrinks, and
// fails on any block its excessive_nesting finds.
//   node scripts/complexity.mjs          check the workspace; exits 1 on any problem (the gate)
//   node scripts/complexity.mjs --hook   Claude Code's after-edit hook: warns, never blocks
import { spawnSync } from 'node:child_process';
import fs from 'node:fs';
import path from 'node:path';

const root = path.resolve(import.meta.dirname, '..');
const baselineFile = path.join(root, 'complexity-baseline.json');
const crates = { archive: 'silk-archive', compiler: 'silk-compiler', stub: 'silk-stub' };
const tip =
  'Too much branching in one function. Break the decisions into small named helpers, return early ' +
  'instead of nesting, and use a lookup table or `match` on data instead of long if/else chains. ' +
  'Fixed an entry? Delete it from complexity-baseline.json (lower it if it only shrank).';
const nestingTip =
  'Too deeply nested. Use guard clauses and early returns (`let ... else`, `?`) instead of wrapping code ' +
  "in if blocks, `continue` to skip items in a loop, and move an inner loop's or closure's body into a " +
  'small named function.';

// Every hit of one lint in clippy's JSON output: its message, primary span, and file with / separators.
function lintHits(clippyJson, lint) {
  return clippyJson
    .split(/\r?\n/)
    .filter((line) => line.startsWith('{'))
    .map((line) => JSON.parse(line))
    .filter(({ reason, message }) => reason === 'compiler-message' && message.code?.code === `clippy::${lint}`)
    .map(({ message }) => {
      const span = message.spans.find((s) => s.is_primary);
      return { message, span, file: span.file_name.replaceAll('\\', '/') };
    });
}

// Every function over the cap in clippy's JSON output, keyed "file::name"; a name repeated in one
// file is told apart by its order there ("name#2").
export function offenders(clippyJson) {
  const byPlace = new Map();
  for (const { message, span, file } of lintHits(clippyJson, 'cognitive_complexity')) {
    const { text, highlight_start, highlight_end } = span.text[0];
    byPlace.set(`${file}:${span.line_start}`, {
      file,
      line: span.line_start,
      name: text.slice(highlight_start - 1, highlight_end - 1),
      n: Number(message.message.match(/\((\d+)\//)[1]),
    });
  }
  return keyed([...byPlace.values()]);
}

// Each { file, line, name, n } as "file::name": n, a name repeated in one file told apart by its order
// there ("name#2").
export function keyed(places) {
  const found = {};
  places.sort((a, b) => a.file.localeCompare(b.file) || a.line - b.line);
  // ponytail: a new over-cap function of the same name above a baselined one takes its key; key by
  // impl or module if that ever lets one through.
  for (const { file, name, n } of places) {
    let key = `${file}::${name}`;
    for (let i = 2; key in found; i++) key = `${file}::${name}#${i}`;
    found[key] = n;
  }
  return found;
}

// Every block nested past the cap, as a problem. No baseline: none was, when the cap came in.
export function nested(clippyJson) {
  const places = lintHits(clippyJson, 'excessive_nesting').map(({ span, file }) => `${file}:${span.line_start}`);
  return [...new Set(places)].map((place) => ({ kind: 'nested', message: `${place} is nested past the cap.` }));
}

// What the found offenders break of the baseline: new or risen entries, and entries left unchanged
// after their function was fixed or simplified.
export function compare(baseline, found) {
  const problems = [];
  for (const [key, was] of Object.entries(baseline)) {
    const now = found[key];
    if (now === undefined) problems.push({ kind: 'fixed', message: `${key} is no longer over the cap: delete its baseline entry.` });
    else if (now > was) problems.push({ kind: 'rose', message: `${key} rose from ${was} to ${now}.` });
    else if (now < was) problems.push({ kind: 'lowered', message: `${key} dropped from ${was} to ${now}: lower it to ${now} in the baseline.` });
  }
  for (const [key, now] of Object.entries(found)) {
    if (!(key in baseline)) problems.push({ kind: 'new', message: `${key} has complexity ${now}, over the cap.` });
  }
  return problems;
}

// clippy's JSON output, or null if it failed. The nesting cap, denied in Cargo.toml, is only warned
// here, so a crate over it still gets linted (and its dependents with it) and this script reports it.
function clippy(args, stderr) {
  const run = spawnSync('cargo', ['clippy', '--locked', '--message-format=json', ...args, '--', '-W', 'clippy::excessive_nesting'], {
    cwd: root,
    encoding: 'utf8',
    maxBuffer: 1 << 28,
    stdio: ['ignore', 'pipe', stderr],
  });
  return run.status === 0 ? run.stdout : null;
}

const baseline = () => JSON.parse(fs.readFileSync(baselineFile, 'utf8'));
export const grows = (p) => p.kind === 'new' || p.kind === 'rose';
const tips = (problems) => [problems.some(grows) && tip, problems.some((p) => p.kind === 'nested') && nestingTip].filter(Boolean);

// The #[allow] or #[expect] attributes in one file's text that name either lint, a group holding them
// or `warnings`, which would hide code from the caps instead of splitting it. Attributes may span
// lines; comments, warn and deny are not looked at.
// ponytail: a `//` inside a string literal hides the rest of its line; parse the Rust if that matters.
export function silencedIn(text) {
  const code = text.replace(/\/\*[\s\S]*?\*\//g, '').replace(/\/\/.*$/gm, '');
  const names = /\b(warnings|cognitive_complexity|excessive_nesting|clippy::(complexity|nursery|all))\b/;
  return [...code.matchAll(/\b(allow|expect)\s*\(([^)]*)\)/g)].filter((m) => names.test(m[2])).map((m) => m[0].replace(/\s+/g, ' '));
}

function silenced() {
  const ls = spawnSync('git', ['ls-files', '--cached', '--others', '--exclude-standard', '--', '*.rs'], { cwd: root, encoding: 'utf8' });
  if (ls.status !== 0 || ls.error) return [{ kind: 'silenced', message: `git ls-files failed: ${ls.stderr ?? ls.error}` }];
  return ls.stdout.split('\n').filter(Boolean).flatMap((file) =>
    silencedIn(fs.readFileSync(path.join(root, file), 'utf8')).map((hit) => ({
      kind: 'silenced',
      message: `${file}: ${hit}: never silence a cap; fix the code.`,
    })),
  );
}

function check() {
  const out = clippy(['--workspace'], 'inherit');
  if (out === null) {
    console.error('complexity: cargo clippy failed');
    return 1;
  }
  const found = offenders(out);
  const problems = [...compare(baseline(), found), ...nested(out), ...silenced()];
  for (const p of problems) console.error(`complexity: ${p.message}`);
  for (const t of tips(problems)) console.error(t);
  if (!problems.length) console.log(`complexity: ${Object.keys(found).length} baseline entries, nothing new, nothing nested.`);
  return problems.length ? 1 : 0;
}

// Lints only the crate the edit touched; any failure (code mid-edit that doesn't build) stays silent.
function hook() {
  const file = JSON.parse(fs.readFileSync(0, 'utf8')).tool_input?.file_path ?? '';
  const [crate] = path.relative(root, file).split(path.sep);
  if (!file.endsWith('.rs') || !crates[crate]) return 0;
  const out = clippy(['-p', crates[crate]], 'ignore');
  const warned = out === null ? [] : [...compare(baseline(), offenders(out)).filter(grows), ...nested(out)];
  if (warned.length) {
    const additionalContext = [...warned.map((p) => p.message), ...tips(warned)].join('\n');
    console.log(JSON.stringify({ hookSpecificOutput: { hookEventName: 'PostToolUse', additionalContext } }));
  }
  return 0;
}

// import.meta.filename is the real path; argv[1] may reach it through a junction.
if (process.argv[1] && !path.relative(fs.realpathSync(process.argv[1]), import.meta.filename)) {
  process.exitCode = process.argv.includes('--hook') ? hook() : check();
}
