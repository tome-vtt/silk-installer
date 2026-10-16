// Caps each non-test function's cognitive complexity (Sonar's algorithm, as Mozilla's
// rust-code-analysis computes it) at 15, holding today's offenders to cognitive-baseline.json, which
// only shrinks.
//   node scripts/cognitive.mjs          check the workspace; exits 1 on any problem (the gate)
//   node scripts/cognitive.mjs --hook   Claude Code's after-edit hook: warns, never blocks
import { spawnSync } from 'node:child_process';
import fs from 'node:fs';
import path from 'node:path';
import { compare, grows, keyed } from './complexity.mjs';

const root = path.resolve(import.meta.dirname, '..');
const baselineFile = path.join(root, 'cognitive-baseline.json');
const cap = 15;
const crates = ['archive', 'compiler', 'stub'];
// The pinned tool; the gate installs it (silk.yml), as does anyone running this by hand.
const tool = 'rust-code-analysis-cli';
const pin = '0.0.25';
const install = `cargo install ${tool} --version ${pin} --locked`;
const tip =
  'Hard to follow. Flatten it: return early (`?`, `let ... else`), pull nested blocks and closure bodies ' +
  'into small named functions, and replace flag variables with plain steps. Fixed an entry? Delete it ' +
  'from cognitive-baseline.json (lower it if it only shrank).';

// Every non-test function over the cap in rust-code-analysis's JSON output (one file per line), keyed
// as complexity.mjs keys clippy's offenders. A closure or inner function counts toward the function
// around it, as Sonar counts it; testStart(file) is the line that file's tests start on.
export function cognitive(rcaJson, testStart) {
  const places = [];
  const visit = (file, space) => {
    const n = space.metrics.cognitive.sum;
    if (space.kind !== 'function') space.spaces.forEach((s) => visit(file, s));
    else if (n > cap && space.start_line < testStart(file)) places.push({ file, line: space.start_line, name: space.name, n });
  };
  for (const line of rcaJson.split(/\r?\n/).filter(Boolean)) {
    const unit = JSON.parse(line);
    visit(unit.name.replaceAll('\\', '/'), unit);
  }
  return keyed(places);
}

// The line a file's tests start on: the #[cfg(test)] (and attributes after it) that opens a `mod`, so
// a #[cfg(test)] helper or `use` above real code hides nothing.
// ponytail: a test module placed above real code would still hide what follows it; the trailing
// module every file here has does not.
export function testStartOf(text) {
  const m = /^#\[cfg\(test\)\]\r?\n(?:\s*#\[.*\]\r?\n)*\s*(?:pub(?:\([^)]*\))?\s+)?mod\s/m.exec(text);
  return m ? text.slice(0, m.index).split('\n').length : Infinity;
}

const testStart = (file) => testStartOf(fs.readFileSync(path.join(root, file), 'utf8'));

// The tool's JSON output for the given paths, or an error message.
function analyse(paths) {
  const version = spawnSync(tool, ['--version'], { encoding: 'utf8' });
  if (version.stdout?.trim() !== `${tool} ${pin}`) return { error: `needs ${tool} ${pin}: ${install}` };
  const args = ['-m', '-O', 'json', '-I', '*.rs', ...paths.flatMap((p) => ['-p', p])];
  const run = spawnSync(tool, args, { cwd: root, encoding: 'utf8', maxBuffer: 1 << 28 });
  return run.status === 0 ? { out: run.stdout } : { error: `${tool} failed: ${run.stderr ?? run.error}` };
}

const baseline = () => JSON.parse(fs.readFileSync(baselineFile, 'utf8'));

function check() {
  const { out, error } = analyse(crates);
  if (error) {
    console.error(`cognitive: ${error}`);
    return 1;
  }
  const found = cognitive(out, testStart);
  const problems = compare(baseline(), found);
  for (const p of problems) console.error(`cognitive: ${p.message}`);
  if (problems.some(grows)) console.error(tip);
  if (!problems.length) console.log(`cognitive: ${Object.keys(found).length} baseline entries, nothing new.`);
  return problems.length ? 1 : 0;
}

// Measures only the edited file; a missing tool stays silent.
function hook() {
  const file = JSON.parse(fs.readFileSync(0, 'utf8')).tool_input?.file_path ?? '';
  const rel = path.relative(root, file);
  if (!file.endsWith('.rs') || !crates.includes(rel.split(path.sep)[0])) return 0;
  const { out } = analyse([rel]);
  const warned = out ? compare(baseline(), cognitive(out, testStart)).filter(grows) : [];
  if (warned.length) {
    const additionalContext = [...warned.map((p) => p.message), tip].join('\n');
    console.log(JSON.stringify({ hookSpecificOutput: { hookEventName: 'PostToolUse', additionalContext } }));
  }
  return 0;
}

// import.meta.filename is the real path; argv[1] may reach it through a junction.
if (process.argv[1] && !path.relative(fs.realpathSync(process.argv[1]), import.meta.filename)) {
  process.exitCode = process.argv.includes('--hook') ? hook() : check();
}
