// Holds clippy's cognitive_complexity offenders to complexity-baseline.json, which only shrinks.
//   node scripts/complexity.mjs          check the workspace; exits 1 on any problem (CI, the gate)
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

// Every function over the cap in clippy's JSON output, keyed "file::name"; a name repeated in one
// file is told apart by its order there ("name#2").
export function offenders(clippyJson) {
  const byPlace = new Map();
  for (const line of clippyJson.split(/\r?\n/)) {
    if (!line.startsWith('{')) continue;
    const { reason, message } = JSON.parse(line);
    if (reason !== 'compiler-message' || message.code?.code !== 'clippy::cognitive_complexity') continue;
    const span = message.spans.find((s) => s.is_primary);
    const { text, highlight_start, highlight_end } = span.text[0];
    byPlace.set(`${span.file_name}:${span.line_start}`, {
      file: span.file_name.replaceAll('\\', '/'),
      line: span.line_start,
      name: text.slice(highlight_start - 1, highlight_end - 1),
      n: Number(message.message.match(/\((\d+)\//)[1]),
    });
  }
  const found = {};
  const places = [...byPlace.values()].sort((a, b) => a.file.localeCompare(b.file) || a.line - b.line);
  for (const { file, name, n } of places) {
    let key = `${file}::${name}`;
    for (let i = 2; key in found; i++) key = `${file}::${name}#${i}`;
    found[key] = n;
  }
  return found;
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

function clippy(args, stderr) {
  const run = spawnSync('cargo', ['clippy', '--locked', '--message-format=json', ...args], {
    cwd: root,
    encoding: 'utf8',
    maxBuffer: 1 << 28,
    stdio: ['ignore', 'pipe', stderr],
  });
  return run.status === 0 ? offenders(run.stdout) : null;
}

const baseline = () => JSON.parse(fs.readFileSync(baselineFile, 'utf8'));

function check() {
  const found = clippy(['--workspace'], 'inherit');
  if (!found) {
    console.error('complexity: cargo clippy failed');
    return 1;
  }
  const problems = compare(baseline(), found);
  for (const p of problems) console.error(`complexity: ${p.message}`);
  if (problems.some((p) => p.kind === 'new' || p.kind === 'rose')) console.error(tip);
  if (!problems.length) console.log(`complexity: ${Object.keys(found).length} baseline entries, nothing new.`);
  return problems.length ? 1 : 0;
}

// Lints only the crate the edit touched; any failure (code mid-edit that doesn't build) stays silent.
function hook() {
  const file = JSON.parse(fs.readFileSync(0, 'utf8')).tool_input?.file_path ?? '';
  const [crate] = path.relative(root, file).split(path.sep);
  if (!file.endsWith('.rs') || !crates[crate]) return 0;
  const found = clippy(['-p', crates[crate]], 'ignore');
  const grown = found && compare(baseline(), found).filter((p) => p.kind === 'new' || p.kind === 'rose');
  if (grown?.length) {
    const additionalContext = [...grown.map((p) => p.message), tip].join('\n');
    console.log(JSON.stringify({ hookSpecificOutput: { hookEventName: 'PostToolUse', additionalContext } }));
  }
  return 0;
}

if (!path.relative(process.argv[1] ?? '', import.meta.filename)) {
  process.exitCode = process.argv.includes('--hook') ? hook() : check();
}
