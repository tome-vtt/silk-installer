import assert from 'node:assert/strict';
import test from 'node:test';
import { cognitive } from './cognitive.mjs';

// One line of `rust-code-analysis-cli -m -O json`: a file's space tree, each space's cognitive sum
// already counting the spaces inside it.
const space = (kind, name, line, cog, spaces = []) => ({ kind, name, start_line: line, spaces, metrics: { cognitive: { sum: cog } } });

test('functions over cognitive 15 are keyed like the clippy offenders; closures count toward theirs, tests do not count', () => {
  const json = [
    JSON.stringify(space('unit', 'stub\\src\\main.rs', 1, 60, [
      space('function', 'main', 131, 36, [space('function', '<anonymous>', 255, 6)]),
      space('function', 'start', 72, 15, [space('function', '<anonymous>', 73, 15)]),
      space('impl', 'Config', 150, 16, [space('function', 'validate', 152, 16)]),
      space('function', 'main', 200, 17),
      space('function', 'big_test', 300, 20),
    ])),
    JSON.stringify(space('unit', 'archive\\src\\lib.rs', 1, 0)),
  ].join('\n');
  const testStart = (file) => (file === 'stub/src/main.rs' ? 290 : Infinity);
  assert.deepEqual(cognitive(json, testStart), {
    'stub/src/main.rs::main': 36,
    'stub/src/main.rs::validate': 16,
    'stub/src/main.rs::main#2': 17,
  });
});
