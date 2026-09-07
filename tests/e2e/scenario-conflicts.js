const assert = require("node:assert/strict");
module.exports = async ({ device, pending, serverFile, log, show }) => {
  const A = device("A");
  const B = device("B");

  log("1. A creates files and syncs");
  A.write("Note.md", "hello\n");
  A.write("P1.tsx", "p1\n");
  A.write("P2.tsx", "p2\n");
  A.write("P3.tsx", "p3\n");
  assert.deepEqual(await A.service.sync(), []);

  log("2. B initial sync");
  assert.deepEqual(await B.service.sync(), []);
  assert.equal(B.read("P1.tsx"), "p1\n");

  log("3. A edits P1, P2, P3 and syncs");
  A.write("P1.tsx", "p1 from A\n");
  A.write("P2.tsx", "p2 from A\n");
  A.write("P3.tsx", "p3 from A\n");
  assert.deepEqual(await A.service.sync(), []);

  log("4. B edits the same three files on its stale base and syncs -> conflicts");
  B.write("P1.tsx", "p1 from B\n");
  B.write("P2.tsx", "p2 from B\n");
  B.write("P3.tsx", "p3 from B\n");
  const first = await B.service.sync();
  show("B sync conflicts", first.map((c) => c.path));
  assert.deepEqual(first.map((c) => c.path), ["P1.tsx", "P2.tsx", "P3.tsx"]);
  assert.deepEqual(Object.keys(pending()), ["P1.tsx", "P2.tsx", "P3.tsx"]);
  const marker = B.read("P1.tsx");
  assert.match(marker, /<<<<<<< server/);

  log("4b. B syncs again without resolving -> conflicts stay, local marker files untouched");
  const again = await B.service.sync();
  show("B sync conflicts", again.map((c) => [c.path, c.reason]));
  assert.deepEqual(again.map((c) => c.path), ["P1.tsx", "P2.tsx", "P3.tsx"]);
  assert.equal(B.read("P1.tsx"), marker, "marker file must not be overwritten");
  assert.equal(B.read("P2.tsx").includes("p2 from B"), true);

  log("4c. pendingConflicts endpoint lists them for B only");
  show("B pending", (await B.service.pendingConflicts()).map((c) => c.path));
  assert.deepEqual((await B.service.pendingConflicts()).map((c) => c.path), ["P1.tsx", "P2.tsx", "P3.tsx"]);
  assert.deepEqual(await A.service.pendingConflicts(), []);

  log("4d. A can still sync normally while B's conflicts are pending");
  A.write("Note.md", "hello again\n");
  assert.deepEqual(await A.service.sync(), []);

  log("5. B resolves only P1 with custom text");
  assert.deepEqual(await B.service.resolveConflicts([{ path: "P1.tsx", kind: "text", content: "p1 resolved\n" }]), []);
  assert.deepEqual(Object.keys(pending()), ["P2.tsx", "P3.tsx"]);
  assert.equal(serverFile("P1.tsx"), "p1 resolved\n");
  assert.equal(B.read("P1.tsx"), "p1 resolved\n");
  assert.equal(B.read("Note.md"), "hello again\n", "resolve response brings B up to date");

  log("6. B syncs -> P2 and P3 still reported");
  const after = await B.service.sync();
  show("B sync conflicts", after.map((c) => c.path));
  assert.deepEqual(after.map((c) => c.path), ["P2.tsx", "P3.tsx"]);
  assert.match(B.read("P2.tsx"), /<<<<<<< server/);

  log("7. B resolves P2 (server side) and P3 (local side) in one call");
  const p2 = B.read("P2.tsx");
  assert.deepEqual(
    await B.service.resolveConflicts([
      { path: "P2.tsx", kind: "text", content: "p2 from A\n" },
      { path: "P3.tsx", kind: "text", content: "p3 from B\n" }
    ]),
    []
  );
  assert.deepEqual(pending(), []);
  assert.equal(serverFile("P2.tsx"), "p2 from A\n");
  assert.equal(serverFile("P3.tsx"), "p3 from B\n");
  void p2;

  log("8. B edits Note.md after resolving; the edit must still be uploaded by the next sync");
  B.write("Note.md", "hello from B\n");
  assert.deepEqual(await B.service.sync(), []);
  assert.equal(serverFile("Note.md"), "hello from B\n");

  log("9. A syncs and receives the resolutions");
  assert.deepEqual(await A.service.sync(), []);
  assert.deepEqual(["P1.tsx", "P2.tsx", "P3.tsx", "Note.md"].map((f) => A.read(f)), ["p1 resolved\n", "p2 from A\n", "p3 from B\n", "hello from B\n"]);

  log("10. locally deleted pending file can be resolved by deleting on the server");
  A.write("Gone.md", "gone\n");
  assert.deepEqual(await A.service.sync(), []);
  assert.deepEqual(await B.service.sync(), []);
  A.write("Gone.md", "gone from A\n");
  assert.deepEqual(await A.service.sync(), []);
  B.write("Gone.md", "gone from B\n");
  assert.deepEqual((await B.service.sync()).map((c) => c.path), ["Gone.md"]);
  require("node:fs").rmSync(require("node:path").join(B.dir, "Gone.md"));
  assert.deepEqual(await B.service.resolveConflicts([{ path: "Gone.md", kind: "delete" }]), []);
  assert.deepEqual(pending(), []);
  assert.equal(serverFile("Gone.md"), null);
  assert.deepEqual(await B.service.sync(), []);
  log("ALL SCENARIO ASSERTIONS PASSED");
};
