import assert from 'node:assert/strict';
import test from 'node:test';
import { compare, nested, offenders } from './complexity.mjs';

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

// One line of `cargo clippy --message-format=json`, as clippy writes an excessive_nesting warning.
const tooNested = (file, line) =>
  JSON.stringify({
    reason: 'compiler-message',
    message: {
      code: { code: 'clippy::excessive_nesting' },
      message: 'this block is too nested',
      spans: [{ is_primary: true, file_name: file, line_start: line, text: [] }],
    },
  });

test('every block nested past the cap is found once, by file and line', () => {
  const json = [
    warning('stub\\src\\main.rs', 131, 'main', 16),
    tooNested('archive\\src\\lib.rs', 536),
    tooNested('archive\\src\\lib.rs', 536),
  ].join('\n');
  assert.deepEqual(nested(json), [{ kind: 'nested', message: 'archive/src/lib.rs:536 is nested past the cap.' }]);
});
