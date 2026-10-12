import { readFileSync } from 'node:fs';
import assert from 'node:assert/strict';
const module = new WebAssembly.Module(readFileSync(process.argv[2]));
assert.deepEqual(WebAssembly.Module.imports(module), [], 'unexpected host dependencies');
const instance = new WebAssembly.Instance(module, {});
assert.equal(typeof instance.exports.main, 'function');
assert.equal(instance.exports.main(0, 0), 0); // Rust assertions trap on failure.
if (process.argv[3] === 'shared') {
  assert.ok(instance.exports.memory.buffer instanceof SharedArrayBuffer, 'memory must be shared');
}
console.log('WASM codec, checksum, malformed-input and exact scalar-byte parity checks passed');
