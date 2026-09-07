interface TextSegment {
  kind: "text";
  text: string;
}

export interface ConflictHunk {
  server: string;
  local: string;
  /** Marker label of the first side, e.g. "server" or, for git-style markers, "HEAD". */
  serverLabel: string;
  /** Marker label of the second side, e.g. "client" or a git commit description. */
  localLabel: string;
}

interface HunkSegment {
  kind: "hunk";
  hunk: ConflictHunk;
}

type ConflictSegment = TextSegment | HunkSegment;

export interface ParsedConflictDocument {
  segments: ConflictSegment[];
  hunks: ConflictHunk[];
  /** True when the markers are git's own (e.g. from a server-side rebase) rather than server/client. */
  generic: boolean;
}

export interface ParseConflictOptions {
  /**
   * Also accept git-style markers with arbitrary labels (`<<<<<<< HEAD` ... `>>>>>>> abc123`).
   * Only enable this for files the server explicitly reported as conflicted, so notes that merely
   * quote a git conflict are never mistaken for one.
   */
  allowGenericMarkers?: boolean;
}

type HunkSelection = { side: "server" | "local" } | { content: string };

export function parseConflictDocument(text: string, options: ParseConflictOptions = {}): ParsedConflictDocument | null {
  const lines = splitLines(text);
  const segments: ConflictSegment[] = [];
  const hunks: ConflictHunk[] = [];
  let common = "";
  let index = 0;
  let generic = false;
  const isStart = options.allowGenericMarkers ? isGenericConflictStartLine : isConflictStartLine;
  const isEnd = options.allowGenericMarkers ? isGenericConflictEndLine : isConflictEndLine;

  while (index < lines.length) {
    const line = lines[index];
    if (!isStart(line)) {
      common += line;
      index += 1;
      continue;
    }

    if (common) {
      segments.push({ kind: "text", text: common });
      common = "";
    }

    const serverLabel = markerLabel(line);
    index += 1;
    let server = "";
    while (index < lines.length && !isConflictSeparatorLine(lines[index])) {
      server += lines[index];
      index += 1;
    }
    if (index >= lines.length) return null;

    index += 1;
    let local = "";
    while (index < lines.length && !isEnd(lines[index])) {
      local += lines[index];
      index += 1;
    }
    if (index >= lines.length) return null;

    const localLabel = markerLabel(lines[index]);
    if (!isConflictStartLine(line) || !isConflictEndLine(lines[index])) generic = true;
    index += 1;
    const hunk: ConflictHunk = { server, local, serverLabel, localLabel };
    hunks.push(hunk);
    segments.push({ kind: "hunk", hunk });
  }

  if (common) segments.push({ kind: "text", text: common });
  return hunks.length > 0 ? { segments, hunks, generic } : null;
}

export function buildResolvedText(parsed: ParsedConflictDocument, choose: (hunk: ConflictHunk) => HunkSelection): string {
  return parsed.segments
    .map((segment) => {
      if (segment.kind === "text") return segment.text;
      const selected = choose(segment.hunk);
      if ("content" in selected) return selected.content;
      return selected.side === "server" ? segment.hunk.server : segment.hunk.local;
    })
    .join("");
}

export function hasConflictMarkers(text: string): boolean {
  const lines = splitLines(text);
  const startIndex = lines.findIndex((line) => isConflictStartLine(line));
  if (startIndex === -1) return false;
  const separatorIndex = lines.findIndex(
    (line, index) => index > startIndex && isConflictSeparatorLine(line)
  );
  if (separatorIndex === -1) return false;
  return lines.some((line, index) => index > separatorIndex && isConflictEndLine(line));
}

export function isConflictStartLine(line: string): boolean {
  return line.trimEnd() === "<<<<<<< server";
}

export function isConflictSeparatorLine(line: string): boolean {
  return line.trimEnd() === "=======";
}

export function isConflictEndLine(line: string): boolean {
  return line.trimEnd() === ">>>>>>> client";
}

export function isGenericConflictStartLine(line: string): boolean {
  return /^<{7}(?: .*)?$/.test(line.trimEnd());
}

export function isGenericConflictEndLine(line: string): boolean {
  return /^>{7}(?: .*)?$/.test(line.trimEnd());
}

function markerLabel(line: string): string {
  return line.trimEnd().slice(7).trim();
}

function splitLines(text: string): string[] {
  if (!text) return [];
  const lines = text.match(/[^\r\n]*(?:\r\n|\n|\r|$)/g) ?? [];
  if (lines[lines.length - 1] === "") lines.pop();
  return lines;
}
