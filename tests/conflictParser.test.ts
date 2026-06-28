import test from "node:test";
import assert from "node:assert/strict";
import { buildResolvedText, parseConflictDocument } from "../src/conflictParser";

test("conflict parser builds whole-file server and local resolutions", () => {
  const parsed = parseConflictDocument("before\n<<<<<<< server\nremote\n=======\nlocal\n>>>>>>> client\nafter\n");

  assert.ok(parsed);
  assert.equal(buildResolvedText(parsed, () => ({ side: "server" })), "before\nremote\nafter\n");
  assert.equal(buildResolvedText(parsed, () => ({ side: "local" })), "before\nlocal\nafter\n");
});

test("conflict parser supports custom per-change text", () => {
  const parsed = parseConflictDocument(
    "A\n<<<<<<< server\nserver one\n=======\nlocal one\n>>>>>>> client\nB\n<<<<<<< server\nserver two\n=======\nlocal two\n>>>>>>> client\nC\n"
  );
  assert.ok(parsed);

  let index = 0;
  const resolved = buildResolvedText(parsed, () => {
    index += 1;
    return index === 1 ? { content: "server\n" } : { content: "merged two\n" };
  });

  assert.equal(resolved, "A\nserver\nB\nmerged two\nC\n");
});

test("conflict parser rejects incomplete markers", () => {
  assert.equal(parseConflictDocument("<<<<<<< server\nremote\n=======\nlocal\n"), null);
});
