# SQL Error Location and SQL File Progress Design

## Scope

This change covers two desktop SQL execution workflows:

1. Show a useful source location when an interactive SQL query fails.
2. Keep SQL file execution status current during long-running statements and make the progress bar stable.

The implementation must preserve existing database error messages and remain compatible with both modern and legacy Windows builds.

## Interactive Query Error Location

### Current behavior

Query failures are represented as a result containing an `Error` column. The result area displays the raw database message, but it does not derive a source line, column, excerpt, or editor selection from that message.

### Proposed behavior

The frontend will derive an optional error location from the raw database message and the SQL text that produced the active result.

When a location can be determined, the result area will show:

- The original error message.
- A one-based line and column label.
- A bounded excerpt containing the affected SQL line.
- A caret marker pointing at the reported column.
- A command that focuses the query editor, selects the affected range, and scrolls it into view.

When a location cannot be determined, the result area will continue to show the original error without inventing a line or column.

### Location parsing

The parser will recognize common location formats returned by supported databases, including combinations such as:

- `line N, column M`
- `at line N`
- `position: P` or `at character P`
- driver messages that expose equivalent one-based offsets

Absolute character positions will be converted to line and column values against the executed SQL. Values will be clamped to the SQL bounds. The parser will return a small structured object rather than UI-ready text so it can be tested independently.

### Source SQL mapping

The error location must use the SQL associated with the active result, not the editor's current contents. If that SQL is a selected statement within a larger editor document, the existing source range metadata will be used to translate the local error offset back to the editor document.

The locate command will emit a dedicated event from `ContentArea` to the query editor owner. The query editor will expose or consume a focused selection range using its existing selection and scroll behavior.

## SQL File Execution Progress

### Current behavior

Progress events are emitted mainly at statement boundaries. A statement that runs for a long time produces no intermediate event. The current frontend percentage is derived from attempted statements divided by the current statement index, so it can jump backward or reach the cap too early. The event carries only a short statement summary and no file byte totals.

### Progress model

`SqlFileProgress` will be extended with optional fields:

- `bytesProcessed`
- `totalBytes`
- `currentStatement`

The fields remain optional for compatibility with existing web and desktop progress producers.

The progress bar will use `bytesProcessed / totalBytes`, clamped to 0-99 while running and set to 100 for a successful terminal event. Displayed progress must never move backward within one execution.

### Heartbeat behavior

While a statement is executing, the backend will emit a progress heartbeat at most once every five seconds. A statement-start event is still emitted immediately, and terminal or statement-completion events are also emitted immediately.

Each heartbeat contains the latest byte position, total file size, counters, elapsed time, and a bounded fragment of the current statement. The heartbeat must stop when the statement completes, execution is cancelled, or the execution task exits.

### Current statement fragment

The current SQL text box will show the current statement, bounded to avoid repeatedly cloning large SQL strings. The fragment limit is 2 KiB of UTF-8 text and will preserve valid character boundaries. Truncated content will end with an ellipsis. This field is for display only and must not alter the SQL sent to the database.

For streaming file execution, byte progress represents bytes consumed from the source file. It is acceptable for a statement to be executing after its bytes have already been read; running progress remains capped below 100 until the terminal event.

## Architecture

The implementation is divided into focused units:

- A frontend SQL error-location parser and excerpt builder.
- Result-area presentation and editor-location wiring.
- Shared SQL file progress fields and fragment helper in `dbx-core`.
- Desktop and web progress producers that supply byte metadata where available.
- SQL file dialog state that keeps progress monotonic and renders the current statement fragment.

No database driver contract changes are required. Raw driver messages remain the source for interactive query location parsing.

## Error Handling

- Invalid or out-of-range error locations fall back to the raw error message.
- Missing byte totals keep the progress bar in its existing indeterminate minimum state.
- Heartbeat emission failures do not cancel SQL execution.
- Cancellation stops heartbeat emission and preserves the latest counters and statement fragment.
- Terminal errors remain visible even when their location cannot be parsed.

## Testing

Frontend tests will cover:

- Line and column message parsing.
- Absolute-position conversion across multi-line SQL.
- Out-of-range and unrecognized errors.
- Excerpt and caret generation.
- Translation from statement-local positions to editor-document selections.
- Monotonic byte-progress calculation and terminal percentages.
- Current statement rendering from heartbeat events.

Rust tests will cover:

- UTF-8-safe 2 KiB statement fragment truncation.
- Progress serialization with byte totals and current statement.
- Five-second heartbeat scheduling behavior using paused Tokio time where practical.
- Immediate statement-start and terminal progress events.

Existing query execution, SQL file import, frontend typecheck, modern Rust, and legacy Rust checks must remain green.

## Non-goals

- Normalizing every database driver's error type in the backend.
- Pre-scanning the complete SQL file to count all statements.
- Sending complete multi-megabyte statements through progress events.
- Changing SQL execution semantics or continue-on-error behavior.
