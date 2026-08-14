# Gateway cold-start reliability design

## Goal

Reduce the chance that the first database connection through DBX Gateway stalls while later attempts succeed, without retrying authentication failures or replaying database traffic.

## Scope

The change covers two verified cold-start pressure points:

1. A local DBX Gateway stream currently gets one chance to create its Main-to-Edge data channel. Failures from that background task are discarded.
2. A newly connected Dameng session starts database-info discovery in the background while connection-root expansion can immediately issue `list_databases`. Both operations can reach a cold JDBC runtime concurrently.

It does not add SQL retries, reconnect an established database stream, or retry certificate, authorization, configuration, and protocol failures.

## Gateway channel retry

For every TCP stream accepted by the local DBX Gateway listener, DBX opens the remote route before reading bytes from the local database client. Route establishment may therefore be retried safely because no JDBC or SQL bytes have been consumed or sent.

The route opener will make at most three attempts with delays of 200 ms and 500 ms. Only transient setup failures are retryable:

- Main TCP connection failure
- Main WebSocket handshake failure caused by a temporary endpoint failure
- connection closed before the stream becomes ready
- `TargetUnavailable`, `EdgeOffline`, or `CapacityExceeded` during route setup

The following failures remain immediate:

- client identity or TLS certificate rejection
- SPKI pin mismatch
- `RouteDenied`
- invalid configuration
- protocol mismatch or malformed response

The local listener task will log the final failure with the Gateway profile ID, Edge ID, target ID, attempt count, and error. It must not log certificate material, credentials, addresses, or database payloads.

## Dameng metadata ordering

Dameng connection setup will not launch database-info discovery concurrently with the first connection-root database listing. When the connection root is opened, `list_databases` completes first; database-info discovery then starts in the background. This keeps optional version metadata non-blocking while avoiding two cold metadata operations against the same newly initialized JDBC runtime.

If the user connects without opening the connection root, database-info discovery may run after the connection reaches its normal idle state. It must never keep the connection spinner active.

Other database drivers retain the current behavior.

## Error handling

- A failed retry attempt closes its partially created WebSocket before waiting.
- Cancellation or tunnel shutdown stops retrying immediately.
- Once relay begins, no automatic retry occurs because application bytes may already have crossed the boundary.
- Metadata failures remain visible through the existing connection error indicator and timeout handling.

## Tests

1. A Rust test proves a transient route-establishment failure is retried and the second attempt can relay data.
2. A Rust test proves `RouteDenied` is not retried.
3. A Rust test proves cancellation during backoff prevents another attempt.
4. A connection-store test proves Dameng does not run database-info discovery concurrently with the first `list_databases` request.
5. Existing Gateway flow, connection-store metadata, type-check, and build checks remain green.

## Compatibility

No configuration migration or protocol version change is required. Retry timing is intentionally fixed and bounded; it is not exposed as another setting.
