export interface SqlErrorLocation {
  line: number;
  column: number;
}

export interface SqlErrorPresentation extends SqlErrorLocation {
  offset: number;
  lineText: string;
  caretColumn: number;
}

export interface SqlDocumentRange {
  from: number;
  to: number;
}

export interface SqlErrorRange {
  from: number;
  to: number;
}

interface SqlLineRange {
  start: number;
  end: number;
  text: string;
}
function toZeroBased(value: string | undefined): number | null {
  if (!value) return null;
  const parsed = Number.parseInt(value, 10);
  if (!Number.isFinite(parsed) || parsed < 1) return null;
  return parsed - 1;
}

export function parseSqlErrorLocation(message: string): SqlErrorLocation | null {
  const lineColumn = /\bline\s+(\d+)\s*[,:\s]\s*column\s+(\d+)\b/i.exec(message) ?? /\bline\s+(\d+)\b[\s\S]{0,80}?\bcol(?:umn)?\s+(\d+)\b/i.exec(message);
  if (lineColumn) {
    const line = toZeroBased(lineColumn[1]);
    const column = toZeroBased(lineColumn[2]);
    if (line != null && column != null) return { line, column };
  }

  const lines = message.split(/\r?\n/);
  for (let index = 0; index < lines.length; index++) {
    const lineMatch = /^LINE\s+(\d+):/i.exec(lines[index] ?? "");
    if (!lineMatch) continue;
    const caretLine = lines.slice(index + 1).find((line) => line.includes("^"));
    const line = toZeroBased(lineMatch[1]);
    const caretIndex = caretLine?.indexOf("^") ?? -1;
    if (line != null && caretIndex >= 0) return { line, column: caretIndex };
  }

  const lineOnly = /\bat\s+line\s+(\d+)\b/i.exec(message);
  const line = toZeroBased(lineOnly?.[1]);
  if (line != null) return { line, column: 0 };

  return null;
}

export function lineColumnToOffset(sql: string, location: SqlErrorLocation): number | null {
  const lines = sqlLineRanges(sql);
  if (location.line < 0 || location.line >= lines.length) return null;
  const line = lines[location.line];
  return Math.min(line.start + Math.max(0, location.column), line.end);
}

function sqlDocumentErrorLocation(message: string): SqlErrorLocation | null {
  const location = parseSqlErrorLocation(message);
  if (!location) return null;

  const lines = message.split(/\r?\n/);
  for (let index = 0; index < lines.length; index++) {
    const lineMatch = /^LINE\s+(\d+):/i.exec(lines[index] ?? "");
    if (!lineMatch || toZeroBased(lineMatch[1]) !== location.line) continue;
    const caretLine = lines.slice(index + 1).find((line) => line.includes("^"));
    const caretIndex = caretLine?.indexOf("^") ?? -1;
    const colonIndex = lines[index]?.indexOf(":") ?? -1;
    if (caretIndex >= 0 && colonIndex >= 0) {
      return { line: location.line, column: Math.max(0, caretIndex - colonIndex - 2) };
    }
  }

  return location;
}

export function buildSqlErrorPresentation(message: string, sql: string): SqlErrorPresentation | null {
  const lines = sqlLineRanges(sql);
  let location = sqlDocumentErrorLocation(message);
  let offset: number | null = null;

  if (location) {
    offset = lineColumnToOffset(sql, location);
    if (offset == null) return null;
  } else {
    const absolutePosition = /\b(?:position\s*:?\s*|at\s+character\s+)(\d+)\b/i.exec(message);
    if (!absolutePosition) return null;
    const oneBasedOffset = Number.parseInt(absolutePosition[1] ?? "", 10);
    if (!Number.isFinite(oneBasedOffset) || oneBasedOffset < 1) return null;
    offset = Math.min(oneBasedOffset - 1, sql.length);
    location = offsetToLineColumn(lines, offset);
  }

  const line = lines[location.line];
  if (!line) return null;
  const column = Math.min(Math.max(0, offset - line.start), line.text.length);
  const excerpt = sqlErrorLineExcerpt(line.text, column);
  return {
    line: location.line,
    column,
    offset,
    lineText: excerpt.text,
    caretColumn: excerpt.caretColumn,
  };
}

export function sqlErrorDocumentRange(presentation: SqlErrorPresentation | null, sourceRange: SqlDocumentRange | undefined): SqlDocumentRange | null {
  if (!presentation || !sourceRange) return null;
  const from = Math.min(Math.max(sourceRange.from + presentation.offset, sourceRange.from), sourceRange.to);
  return { from, to: Math.min(from + 1, sourceRange.to) };
}

function offsetToLineColumn(lines: SqlLineRange[], offset: number): SqlErrorLocation {
  for (let index = 0; index < lines.length; index++) {
    const line = lines[index];
    if (offset <= line.end || index === lines.length - 1) {
      return { line: index, column: Math.min(Math.max(0, offset - line.start), line.text.length) };
    }
  }
  return { line: 0, column: 0 };
}

function sqlLineRanges(sql: string): SqlLineRange[] {
  const lines: SqlLineRange[] = [];
  let start = 0;
  let index = 0;
  while (index < sql.length) {
    if (sql[index] !== "\r" && sql[index] !== "\n") {
      index += 1;
      continue;
    }
    lines.push({ start, end: index, text: sql.slice(start, index) });
    if (sql[index] === "\r" && sql[index + 1] === "\n") index += 1;
    index += 1;
    start = index;
  }
  lines.push({ start, end: sql.length, text: sql.slice(start) });
  return lines;
}

function sqlErrorLineExcerpt(lineText: string, column: number): { text: string; caretColumn: number } {
  const maxContentLength = 160;
  if (lineText.length <= maxContentLength) return { text: lineText, caretColumn: column };

  const start = Math.max(0, Math.min(column - Math.floor(maxContentLength / 2), lineText.length - maxContentLength));
  const end = Math.min(lineText.length, start + maxContentLength);
  const prefix = start > 0 ? "..." : "";
  const suffix = end < lineText.length ? "..." : "";
  return {
    text: `${prefix}${lineText.slice(start, end)}${suffix}`,
    caretColumn: prefix.length + column - start,
  };
}

function oracleInvalidIdentifierRange(sql: string, message: string, position: number): SqlErrorRange | null {
  const identifierMatch = /\bORA-00904:\s*"((?:""|[^"])*)"\s*:\s*invalid identifier\b/i.exec(message);
  if (!identifierMatch?.[1]) return null;

  const reportedIdentifier = identifierMatch[1].replace(/""/g, '"');
  const identifierPattern = /"(?:""|[^"])*"|[\p{L}_][\p{L}\p{N}_$#]*/gu;
  let bestRange: SqlErrorRange | null = null;
  let bestDistance = Number.POSITIVE_INFINITY;

  for (const match of sql.matchAll(identifierPattern)) {
    const token = match[0];
    const tokenStart = match.index;
    const quoted = token.startsWith('"');
    const identifier = quoted ? token.slice(1, -1).replace(/""/g, '"') : token;
    if (quoted ? identifier !== reportedIdentifier : identifier.toUpperCase() !== reportedIdentifier.toUpperCase()) continue;

    const from = tokenStart + (quoted ? 1 : 0);
    const to = tokenStart + token.length - (quoted ? 1 : 0);
    const distance = position < from ? from - position : position > to ? position - to : 0;
    if (distance >= bestDistance) continue;
    bestDistance = distance;
    bestRange = { from, to };
  }

  return bestRange;
}

export function sqlErrorDecorationRange(sql: string, message: string): SqlErrorRange | null {
  const location = sqlDocumentErrorLocation(message);
  if (location) {
    const offset = lineColumnToOffset(sql, location);
    if (offset == null || offset >= sql.length) return null;
    return { from: offset, to: offset + 1 };
  }

  // Oracle Agent reports a zero-based absolute offset. For qualified invalid
  // identifiers it can point later in the selector, so prefer the named token.
  const positionMatch = /\berror\s+occur(?:red)?\s+at\s+position\s*:\s*(\d+)\b/i.exec(message);
  if (!positionMatch?.[1]) return null;
  const position = Number.parseInt(positionMatch[1], 10);
  if (!Number.isSafeInteger(position) || position < 0 || position >= sql.length) return null;

  return oracleInvalidIdentifierRange(sql, message, position) ?? { from: position, to: position + 1 };
}
