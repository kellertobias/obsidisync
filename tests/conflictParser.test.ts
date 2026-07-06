import test from "node:test";
import assert from "node:assert/strict";
import { buildResolvedText, hasConflictMarkers, parseConflictDocument } from "../src/conflictParser";

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

test("conflict parser ignores generic merge-conflict-style text without the server/client labels", () => {
  const notAboutSync =
    "Here is an example of a git merge conflict:\n<<<<<<< HEAD\nmy change\n=======\ntheir change\n>>>>>>> feature-branch\n";
  assert.equal(parseConflictDocument(notAboutSync), null);
});

test("hasConflictMarkers ignores a note that merely mentions generic merge-conflict text", () => {
  const noteAboutGit =
    "How to resolve a merge conflict:\n<<<<<<< HEAD\nmy change\n=======\ntheir change\n>>>>>>> feature-branch\n";
  assert.equal(hasConflictMarkers(noteAboutGit), false);
});

test("hasConflictMarkers detects a real server-generated conflict document", () => {
  const realConflict = "<<<<<<< server\nremote\n=======\nlocal\n>>>>>>> client\n";
  assert.equal(hasConflictMarkers(realConflict), true);
});
