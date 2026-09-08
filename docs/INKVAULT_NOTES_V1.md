# InkVault notes v1

The server advertises `inkVaultNotesV1` in `/v1/server/info`. Native clients send
`X-ObsidiSync-Client-Features: inkVaultNotesV1` on sync, history and blob requests.
The feature header selects representation, not authorization: existing bearer
identity and vault access checks still apply. Ordinary sync and WebDAV show the
visible PDF only. They cannot write, delete or move a managed PDF independently.

## Wire protocol

Use the existing staged upload API, then POST the standard `SyncRequest` to
`/v1/users/{user}/vaults/{vault}/sync`. Ordinary files and InkNotes use separate
requests. A source request contains exactly one document, including its
`manifest.json` and changed pages/assets. Unchanged source is loaded from the
current package. InkVault currently uploads the complete package. Every upsert
requires SHA-256; `fileContent: "reference"` keeps responses small. `baseHead`
identifies the revision the device edited. Unrelated server changes do not
conflict, but any changed member of the pair does. The server returns all members
of a changed/conflicted pair, including deletion tombstones. It does not advance
the client merge base on a conflict. A replay of the same source revision is a
no-op even though the server has replaced the manifest's `renderRevision`.

POST the same request shape to `/v1/users/{user}/vaults/{vault}/inkvault/resolve`
to keep a local pair after review. Here `baseHead` must equal the exact server
head shown to the user; a later update returns HTTP 409. Send the complete source
and advance `sourceRevision` above the received revision. The PDF is always
regenerated. Choosing the received pair is a local, atomic adoption of all the
verified conflict blobs; it does not rewrite server content.

Source layout:

```
.inkvault/notes/{documentUUID}/manifest.json
.inkvault/notes/{documentUUID}/pages/{pageUUID}.cbor
.inkvault/notes/{documentUUID}/assets/{sha256}.{extension}
```

The manifest contains schemaVersion 1, documentId, pdfPath, title, created,
modified, monotonically increasing sourceRevision, server-derived renderRevision,
and ordered pages with id, width, height, orientation, template and sha256.
Units are integer micrometres, milliseconds, and thousandths for pressure/tilt.
CBOR uses canonical definite-length string-keyed maps and integers. Unknown
supported CBOR values and JSON fields remain in the stored source. The renderer
rejects unsupported encodings instead of interpreting them approximately.

New notes use A4 portrait/landscape pages, up to 500 pages. Annotation manifests
also specify basePdfHash and basePdfRevision; the server verifies the original
PDF at that historical commit. For an offline import, a null basePdfRevision is
anchored to the current server head only after its PDF hash is verified. Base identity and page IDs/order/dimensions stay
fixed. Each render starts from that original PDF, preserving its content and
adding vector ink, rather than repeatedly flattening a previous rendition.

## Publication and recovery

Every hidden source file and the generated PDF is an immutable SHA-addressed
binary object. Their pointers live together in the Git binary manifest. Before
publication the server validates all source, renders the PDF, and flushes new
objects. An alternate Git index builds one commit containing the new ledger.
A flushed recovery journal records old/new heads and the ledger, then a compare-
and-swap ref update publishes the commit. The working ledger and index are
reconciled before the journal is removed. The next vault operation after a crash
rolls the transaction forward and checks the journal against the committed tree.
An unexpected head stops recovery rather than overwriting an unrelated commit.

`inkvault-render.json` beside the vault records the latest render as pending,
failed (with the source revision/error), or ready (with the committed head).
Failed renders leave the previous pair and Git head unchanged. Completed upload
objects and client outbox data remain available for retry. A failed remote push
also leaves the committed pair intact for retry. Once a vault has InkNotes,
remote updates must be fast-forwards; divergent Git histories require explicit
operator reconciliation instead of a text merge of the binary ledger.

Deletion requires deletion of every source path, including the manifest; the
server deletes the visible PDF in the same revision. Git history and the immutable
object store retain earlier complete pairs.

## Rendering limits and current boundaries

- Vector pen/pressure strokes and translucent markers remain vector PDF content.
- PDF templates remain PDF form objects; PNG/JPEG and SVG path artwork are bounded
  raster assets, cached when reused. Host fonts and external resources are not
  used. SVG text must be converted to paths; nested SVG images and foreignObject
  are rejected explicitly.
- Limits: 1 MiB manifest, 8 MiB CBOR/page, 64 MiB active vector source, 256 MiB
  package, 4096 package files, 128 MiB accumulated decoded image streams, and
  256 MiB output PDF. Publications are serialized across vaults to bound concurrent
  render allocations. These are safeguards, not a full load-test result.
- Encrypted PDFs and non-default PDF UserUnit are rejected. Annotation rotation
  and CropBox/MediaBox are honored. Managed PDF renaming is currently rejected;
  create a separate document to change its visible path.
- No automatic divergent-remote reconciliation, blob garbage collection,
  renderer migration, or production deployment is included.

## Verification

`cargo test --manifest-path rust-server/Cargo.toml` exercises the existing server
suite. `--test inkvault_tests` covers lost responses, incomplete staged uploads,
stale-pair conflicts, compare-and-swap resolution, visibility/WebDAV guards,
recovery before/after ref publication, original PDF preservation, paired deletion,
deterministic PNG/SVG/PDF-template rendering, and 500 mixed-orientation pages.
Android also tests journal persistence and whole-pair conflict adoption on BOOX.
