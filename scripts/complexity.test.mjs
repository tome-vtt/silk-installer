import assert from 'node:assert/strict';
import { execFileSync } from 'node:child_process';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import test from 'node:test';
import { compare, nested, offenders, silenced, silencedIn } from './complexity.mjs';

// One line of `cargo clippy --message-format=json`, as clippy writes a cognitive_complexity warning.
const warning = (file, line, name, n) =>
  JSON.stringify({
    reason: 'compiler-message',
    message: {
      code: { code: 'clippy::cognitive_complexity' },
      message: `the function has a cognitive complexity of (${n}/4)`,
      spans: [{ is_primary: true, file_name: file, line_start: line, text: [{ text: `fn ${name}() {`, highlight_start: 4, highlight_end: 4 + name.length }] }],
    },
  });

// The same, as clippy writes an excessive_nesting warning.
const tooNested = (file, line) =>
  JSON.stringify({
    reason: 'compiler-message',
    message: {
      code: { code: 'clippy::excessive_nesting' },
      message: 'this block is too nested',
      spans: [{ is_primary: true, file_name: file, line_start: line, text: [] }],
    },
  });

test('offenders are keyed by file and function, a repeated name by its order in the file', () => {
  const json = [
    '{"reason":"compiler-artifact"}',
    warning('stub\\src\\main.rs', 131, 'main', 16),
    warning('stub\\src\\a.rs', 90, 'new', 6),
    warning('stub\\src\\a.rs', 10, 'new', 5),
    warning('stub\\src\\a.rs', 10, 'new', 5),
  ].join('\n');
  assert.deepEqual(offenders(json), { 'stub/src/main.rs::main': 16, 'stub/src/a.rs::new': 5, 'stub/src/a.rs::new#2': 6 });
});

test('an unchanged tree passes', () => {
  assert.deepEqual(compare({ 'a.rs::f': 7 }, { 'a.rs::f': 7 }), []);
});

test('a new offender and a risen entry fail', () => {
  const kinds = compare({ 'a.rs::f': 7 }, { 'a.rs::f': 8, 'a.rs::g': 5 }).map((p) => p.kind);
  assert.deepEqual(kinds, ['rose', 'new']);
});

test('a fixed entry must be removed and a shrunken one lowered', () => {
  const problems = compare({ 'a.rs::f': 7, 'a.rs::g': 6 }, { 'a.rs::g': 5 });
  assert.deepEqual(problems.map((p) => p.kind), ['fixed', 'lowered']);
  assert.match(problems[0].message, /delete/i);
  assert.match(problems[1].message, /lower it to 5/);
});

test('every block nested past the cap is found once, by file and line', () => {
  const json = [
    warning('stub\\src\\main.rs', 131, 'main', 16),
    tooNested('archive\\src\\lib.rs', 536),
    tooNested('archive\\src\\lib.rs', 536),
  ].join('\n');
  assert.deepEqual(nested(json), [{ kind: 'nested', message: 'archive/src/lib.rs:536 is nested past the cap.' }]);
});

test('allow and expect of a cap, its groups or warnings are silencing, on one line or several', () => {
  for (const attr of [
    '#[allow(warnings)]',
    '#![allow(warnings)]',
    '#[allow(\n    clippy::nursery\n)]',
    '#[expect(clippy::cognitive_complexity, reason = "x")]',
    '#[cfg_attr(test, allow(clippy::excessive_nesting))]',
    '#[allow(clippy::all)]',
  ]) {
    assert.equal(silencedIn(`${attr}\nfn f() {}`).length, 1, attr);
  }
});

test('comments, warn, deny and other lints are not silencing', () => {
  const text = '// allow(clippy::all) is refused\n/* #[allow(warnings)] */\n#![warn(clippy::nursery)]\n#[deny(warnings)]\n#[allow(dead_code)]\n';
  assert.deepEqual(silencedIn(text), []);
});

test('the attribute scan skips docs but reports the same Rust outside docs', (t) => {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), 'silk-complexity-'));
  t.after(() => fs.rmSync(root, { recursive: true, force: true }));
  execFileSync('git', ['init', '--quiet', root]);
  fs.mkdirSync(path.join(root, 'docs'));
  const rust = '#[allow(clippy::cognitive_complexity)]\nfn f() {}\n';
  fs.writeFileSync(path.join(root, 'docs', 'x.rs'), rust);
  assert.deepEqual(silenced(root), []);
  fs.writeFileSync(path.join(root, 'x.rs'), rust);
  const expected = [{ kind: 'silenced', message: 'x.rs: allow(clippy::cognitive_complexity): never silence a cap; fix the code.' }];
  assert.deepEqual(silenced(root), expected);
  execFileSync('git', ['-C', root, 'add', '.']);
  assert.deepEqual(silenced(root), expected);
});
