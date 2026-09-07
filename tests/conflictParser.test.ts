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

test("conflict parser accepts git-style markers only when explicitly allowed", () => {
  const rebaseConflict = "top\n<<<<<<< HEAD\nremote branch\n=======\npending sync\n>>>>>>> 1234abc (sync: iPhone)\nbottom\n";
  assert.equal(parseConflictDocument(rebaseConflict), null);

  const parsed = parseConflictDocument(rebaseConflict, { allowGenericMarkers: true });
  assert.ok(parsed);
  assert.equal(parsed.generic, true);
  assert.equal(parsed.hunks[0].serverLabel, "HEAD");
  assert.equal(parsed.hunks[0].localLabel, "1234abc (sync: iPhone)");
  assert.equal(buildResolvedText(parsed, () => ({ side: "server" })), "top\nremote branch\nbottom\n");
  assert.equal(buildResolvedText(parsed, () => ({ side: "local" })), "top\npending sync\nbottom\n");
});

test("conflict parser reports server/client markers as non-generic with their labels", () => {
  const parsed = parseConflictDocument("<<<<<<< server\nremote\n=======\nlocal\n>>>>>>> client\n", { allowGenericMarkers: true });
  assert.ok(parsed);
  assert.equal(parsed.generic, false);
  assert.equal(parsed.hunks[0].serverLabel, "server");
  assert.equal(parsed.hunks[0].localLabel, "client");
});
