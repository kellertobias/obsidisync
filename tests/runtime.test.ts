import test from "node:test";
import assert from "node:assert/strict";
import { createClientId, getDeviceName } from "../src/runtime";

test("createClientId uses crypto.randomUUID when available", () => {
  assert.equal(createClientId({ randomUUID: () => "11111111-2222-4333-8444-555555555555" }), "11111111-2222-4333-8444-555555555555");
});

test("createClientId falls back to getRandomValues and returns an RFC 4122 v4 UUID", () => {
  const id = createClientId({
    getRandomValues: (array) => {
      for (let index = 0; index < array.length; index += 1) array[index] = index;
      return array;
    }
  });

  assert.match(id, /^[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/);
  assert.equal(id, "00010203-0405-4607-8809-0a0b0c0d0e0f");
});

test("getDeviceName prefers configured names and trims whitespace", () => {
  assert.equal(getDeviceName("  Keller iPad  ", { platform: "MacIntel" }), "Keller iPad");
});

test("getDeviceName falls back through platform, user agent, and generic label", () => {
  assert.equal(getDeviceName("", { platform: "iPhone", userAgent: "ignored" }), "iPhone");
  assert.equal(getDeviceName("", { platform: "", userAgent: "ObsidianMobile/1.0" }), "ObsidianMobile/1.0");
  assert.equal(getDeviceName("", { platform: "", userAgent: "" }), "Obsidian device");
});
