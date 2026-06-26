import test from "node:test";
import assert from "node:assert/strict";
import { arrayBufferToBase64, base64ToArrayBuffer } from "../src/base64";

function bytesFrom(buffer: ArrayBuffer): number[] {
  return [...new Uint8Array(buffer)];
}

test("round trips every byte value", () => {
  const input = Uint8Array.from({ length: 256 }, (_, index) => index);
  const encoded = arrayBufferToBase64(input.buffer);
  const decoded = base64ToArrayBuffer(encoded);

  assert.deepEqual(bytesFrom(decoded), bytesFrom(input.buffer));
});

test("encodes large buffers in chunks", () => {
  const input = Uint8Array.from({ length: 100_000 }, (_, index) => index % 251);
  const encoded = arrayBufferToBase64(input.buffer);
  const decoded = base64ToArrayBuffer(encoded);

  assert.equal(encoded, Buffer.from(input).toString("base64"));
  assert.deepEqual(bytesFrom(decoded), [...input]);
});
