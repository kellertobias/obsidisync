interface TextSegment {
  kind: "text";
  text: string;
}

export interface ConflictHunk {
  server: string;
  local: string;
}

interface HunkSegment {
  kind: "hunk";
  hunk: ConflictHunk;
}

type ConflictSegment = TextSegment | HunkSegment;

export interface ParsedConflictDocument {
  segments: ConflictSegment[];
  hunks: ConflictHunk[];
}

type HunkSelection = { side: "server" | "local" } | { content: string };

export function parseConflictDocument(text: string): ParsedConflictDocument | null {
  const lines = splitLines(text);
  const segments: ConflictSegment[] = [];
  const hunks: ConflictHunk[] = [];
  let common = "";
  let index = 0;

  while (index < lines.length) {
    const line = lines[index];
    if (!isConflictStartLine(line)) {
      common += line;
      index += 1;
      continue;
    }

    if (common) {
      segments.push({ kind: "text", text: common });
      common = "";
    }

    index += 1;
    let server = "";
    while (index < lines.length && !isConflictSeparatorLine(lines[index])) {
      server += lines[index];
      index += 1;
    }
    if (index >= lines.length) return null;

    index += 1;
    let local = "";
    while (index < lines.length && !isConflictEndLine(lines[index])) {
      local += lines[index];
      index += 1;
    }
    if (index >= lines.length) return null;

    index += 1;
    const hunk = { server, local };
    hunks.push(hunk);
    segments.push({ kind: "hunk", hunk });
  }

  if (common) segments.push({ kind: "text", text: common });
  return hunks.length > 0 ? { segments, hunks } : null;
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

function splitLines(text: string): string[] {
  if (!text) return [];
  const lines = text.match(/[^\r\n]*(?:\r\n|\n|\r|$)/g) ?? [];
  if (lines[lines.length - 1] === "") lines.pop();
  return lines;
}
