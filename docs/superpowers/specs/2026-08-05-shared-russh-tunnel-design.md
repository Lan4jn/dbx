# Shared russh Tunnel Sessions Design

Date: 2026-08-05

## Summary

DBX currently establishes a separate `russh` SSH session for every enabled SSH transport layer on every database connection. A connection may reference a tunnel profile from Settings > Tunnels, but that reference only shares configuration: after profile resolution, each connection still owns a separate SSH handshake, keepalive loop, local listener, and reconnect loop.

This change gives SSH tunnel profiles an optional shared-session mode. Connections that reference the same shared profile authenticate once and reuse one real SSH-2.0 session while retaining independent local listener ports and independent `direct-tcpip` targets. The connection dialog also replaces the current combined profile dropdown with an explicit choice between a Settings-managed global SSH configuration and a configuration stored only on the current database connection.

OpenSSH process integration, protocol padding, and claims that the SSH gateway cannot identify port forwarding are outside this design. Both shared and non-shared tunnels continue to use the existing `russh` implementation and host-key verification.

## Goals

- Let an SSH tunnel profile opt into sharing one authenticated SSH session across all database connections that reference it.
- Let each referencing database connection forward to its own database host and port through that shared session.
- Support the same SSH authentication methods, host-key checks, keepalive behavior, reconnect behavior, and SSH chains that DBX supports today.
- Make the connection-level choice explicit: use a global SSH profile from Settings > Tunnels, or use SSH settings stored only on the current connection.
- Preserve all existing saved connections and tunnel profiles without migration.
- Ensure concurrent connection attempts for one shared profile perform at most one initial SSH handshake.

## Non-goals

- Bundling or invoking an OpenSSH executable.
- Making `russh` imitate a particular OpenSSH version or algorithm fingerprint.
- Hiding `direct-tcpip` channel targets from the SSH server; the SSH server necessarily sees the forwarding request.
- Traffic padding or resistance to statistical traffic analysis.
- Sharing connection-local SSH configurations. Only Settings-managed profiles can opt into cross-connection sharing.
- Sharing proxy or HTTP tunnel sessions.
- Combining different SSH profiles merely because their host and credentials happen to match.

## Existing Design to Reuse

- `crates/dbx-core/src/db/ssh_tunnel.rs` owns SSH authentication, host-key verification, local listeners, channel forwarding, keepalives, reconnects, and `TunnelManager` lifecycle.
- `crates/dbx-core/src/connection.rs::resolved_transport_layers` resolves `profile_id` references at connection time and deliberately preserves the reference's `profile_id`.
- `crates/dbx-core/src/models/connection.rs::TransportLayerConfig::resolved_from_profile` copies the current profile while retaining the connection layer ID, enabled state, and profile ID.
- `apps/desktop/src/lib/connection/tunnelProfiles.ts` creates profiles and converts between profile references and self-contained connection layers.
- `apps/desktop/src/components/connection/ConnectionDialog.vue` already supports attaching and detaching a profile reference, although the interaction is currently a single dropdown.
- `apps/desktop/src/components/connection/TunnelProfileManager.vue` is the Settings > Tunnels editor.
- Existing in-process SSH test servers in `crates/dbx-core/src/db/ssh_tunnel.rs` can count handshakes and verify channel forwarding without external infrastructure.

## Configuration Model

Add one backward-compatible field to `SshTunnelConfig`:

```rust
#[serde(default)]
pub share_session: bool,
```

The matching TypeScript field is optional:

```typescript
share_session?: boolean;
```

The default is `false`, so old tunnel profiles and connection-local configurations retain their current one-session-per-layer behavior.

The field is meaningful only when all of the following are true:

- the resolved transport layer is SSH;
- `share_session` is true;
- `profile_id` is non-empty, proving that the layer references a Settings-managed profile.

A connection-local SSH layer always receives a private session even if externally supplied JSON contains `share_session: true`. Normalization and profile detachment set the field to `false` so saved data reflects the effective behavior.

Cloud sync needs no schema migration because tunnel profiles are serialized as JSON. The new non-secret boolean remains in both ordinary and encrypted snapshots; existing secret scrubbing behavior is unchanged.

## Connection Dialog

For a selected SSH transport layer, replace the current profile dropdown with a radio group:

- **Use global tunnel configuration**
- **Use this connection's configuration**

When global configuration is selected:

- show a second selector containing only SSH profiles from Settings > Tunnels;
- store a reference stub containing the layer identity, enabled state, display name, and `profile_id`;
- hide connection-local host, port, authentication, key, timeout, and LAN exposure controls;
- show the selected profile summary and a command that opens tunnel maintenance;
- reject save/test when no SSH profile is selected.

When the current connection's configuration is selected:

- show the existing SSH configuration controls;
- store the complete SSH configuration on the database connection;
- remove `profile_id` and force `share_session` to false;
- when switching from a valid global profile, initialize the local fields from that profile, matching the existing detach behavior.

Missing referenced profiles continue to fail closed. The dialog shows the existing missing-profile warning, and backend connection attempts return the existing missing-profile error rather than bypassing the tunnel.

Proxy and HTTP tunnel layers retain their existing profile selector in this change. The new radio interaction applies only to SSH, as requested.

## Tunnel Profile Manager

The SSH profile editor adds a checkbox labeled "Share one SSH session across connections". It is stored as `share_session` and is available for every SSH authentication method.

The hint text states that each database connection keeps its own local forwarding port and target. Disabling the option does not disconnect already-open database pools; it applies when connections next establish their transport layers.

New SSH profiles default to sharing disabled. Proxy and HTTP profiles do not show the option.

## Backend Architecture

### Session and Forwarding Separation

Split the current tunnel entry, which couples one SSH session to one local listener, into two lifecycle levels:

1. `SshSessionEntry` owns authentication configuration, the current `russh::client::Handle`, keepalive/reconnect supervision, and a reference count.
2. `SshForwardEntry` owns one local TCP listener, one remote target, its forwarding task, and a lease on an SSH session entry.

`TunnelManager` keeps separate maps for sessions and forwards. A database transport layer remains addressed by its existing layer ID (`<connection_id>:transport:<index>`), so stop and reconnect callers outside the SSH module do not need a new lifecycle contract.

### Session Keys

For a shared profile, derive the session key from:

- `profile_id`;
- a SHA-256 fingerprint of the effective SSH connection/authentication configuration.

The fingerprint includes SSH host, port, user, authentication method, password, key path, key passphrase, agent settings, and connection timeout. The map key stores only the digest, never raw credentials. Target database host/port and `expose_lan` are excluded because they belong to individual forwards, not the authenticated SSH session.

Including a configuration fingerprint allows a modified profile to create a new session immediately while existing forwards finish on the old session. The old session closes when its final lease is released.

For a non-shared layer, use its unique transport layer ID as the session key. This preserves current isolation even when two local configurations contain identical credentials.

### Concurrent Startup

Session creation is serialized per session key. The first caller performs host-key verification and authentication; concurrent callers wait and then lease the resulting session. A failed initial connection is not inserted into the active-session map, and all waiting callers receive the failure without leaving a stale entry.

Forward creation remains serialized per transport layer ID. Repeated starts for the same active layer return its existing local port, preserving the current idempotent API.

### Forwarding

Each forward binds its own `127.0.0.1` listener, or `0.0.0.0` when that layer's effective `expose_lan` setting is enabled. For every accepted local TCP connection, the forward asks its leased session to open a `direct-tcpip` channel to that forward's remote host and port, then copies bytes bidirectionally using the existing channel loop.

Different database targets therefore share only the encrypted SSH connection. Their local ports, remote targets, database pools, cancellation, and failures remain independent.

SSH chains continue to create one forward per hop. A hop that references a shared profile may reuse its authenticated session even when its target is a different next-hop SSH endpoint for another database connection. Connection-local hops remain private.

### Keepalive and Reconnect

Move keepalive and reconnect ownership from the per-forward loop to `SshSessionEntry`:

- one supervisor sends the existing periodic ping for the shared session;
- a closed session or failed ping triggers one reconnect sequence with the existing exponential backoff and retry limit;
- reconnect is serialized so multiple forwards cannot authenticate in parallel;
- after reconnection, new channels use the replacement handle;
- an in-flight channel that was lost fails normally and is not silently replayed, because replaying database protocol bytes could duplicate operations;
- local listeners remain alive while the session reconnects, preserving the current stable-local-port behavior for subsequent connection attempts.

If reconnect retries are exhausted, the session becomes terminal. New channel requests fail with the final SSH error, and a later fresh transport-layer start may create a new session after the stale entry is removed.

### Shutdown and Reference Counting

Starting a forward acquires one session lease. Removing or aborting that forward releases exactly one lease. When the last lease is released:

- abort the session supervisor;
- close the current `russh` handle;
- remove its session and start-lock entries.

Stopping one database connection removes only its forwarding entries. Other connections using the same profile retain the SSH session and continue operating. `stop_all_tunnels` aborts all forwards first and then all sessions.

Profile testing always uses a private probe session, even when the profile has sharing enabled. A Settings test must neither reuse nor interrupt a live shared session.

## Security Properties

- The outer connection remains a real SSH-2.0 connection implemented by `russh`; database protocol bytes travel only inside encrypted SSH channels on the external network path.
- Existing known-hosts checks, changed-key rejection, explicit TOFU prompts, and credential ordering remain unchanged and occur once per authenticated shared session.
- Sharing does not mix decrypted database bytes between targets: every local TCP stream owns one SSH channel with an explicit remote endpoint.
- Secrets are never included verbatim in session map keys or logs.
- The SSH server can still identify `direct-tcpip` forwarding and its requested targets. This feature does not claim otherwise.
- The gateway's outbound database socket belongs to its SSH server process, not to DBX, and does not carry DBX process metadata. However, terminating SSH exposes the forwarded database byte stream to the gateway. Without database-level TLS, gateway-side capture or host inspection can read database handshakes and SQL and may infer a driver or DBX-generated query pattern. With end-to-end database TLS enabled, the gateway still sees the database destination and TLS traffic metadata but cannot read the database payload; the database server itself necessarily can.

## Error Handling

- Missing global profile: retain the existing fail-closed connection error.
- Profile type mismatch: retain the existing validation error.
- Initial shared authentication failure: fail every waiter and leave no active session.
- Per-target channel-open failure: fail only the affected local stream and log the target-safe error; do not terminate unrelated forwards.
- Shared-session reconnect exhaustion: surface a stable SSH unavailable error to all new channel attempts until the stale session is recreated.
- Local listener bind failure: release the acquired session lease before returning the error.

## Compatibility and Rollout

- `share_session` uses serde/default and an optional TypeScript field; no SQLite migration is required.
- Existing profiles default to non-shared behavior.
- Existing profile references keep resolving through `profile_id`.
- Existing connection-local SSH layers remain private.
- No updater configuration, public key, release endpoint, or version number changes are part of this feature.

## Testing

Backend model/storage tests:

- old SSH JSON without `share_session` deserializes to false;
- tunnel profile storage and cloud-sync round trips preserve the flag;
- profile resolution preserves `profile_id` and applies the profile's sharing flag;
- detached/local configurations force sharing off.

SSH manager tests using the existing local SSH test server:

- two layer IDs using one shared profile and different remote targets produce one SSH handshake;
- both local ports reach their respective target echo servers;
- sharing disabled produces two SSH handshakes;
- connection-local layers remain separate even if `share_session` is supplied;
- concurrent shared starts perform one handshake;
- stopping one forward does not close the other;
- stopping the final forward closes and removes the shared session;
- failed listener setup releases its session lease;
- reconnect replaces the shared handle once and keeps forwarding listeners available;
- an unknown or changed host key retains the current fail-closed behavior.

Frontend tests:

- new SSH profiles default to `share_session: false`;
- the profile manager shows and persists the sharing checkbox only for SSH profiles;
- an SSH layer renders the global/current radio group;
- global mode lists only SSH profiles and stores a reference stub;
- current mode detaches profile values and forces sharing off;
- missing global profile remains visible and blocks a silent direct connection;
- proxy and HTTP tunnel profile selection behavior is unchanged.

## Acceptance Criteria

- With one shared SSH profile selected by two database connections targeting different databases, packet capture and the test server observe one SSH handshake and two independent `direct-tcpip` targets.
- Both database connections can operate concurrently through their own stable local ports.
- Closing either database connection does not interrupt the other.
- Closing the final reference terminates the shared SSH session.
- Disabling profile sharing restores one SSH session per connection.
- Selecting current-connection configuration never joins a shared session.
- Existing saved connections load and connect with their previous behavior.
