# P2P Wire-Traffic Capture Contract

Version: 1.0.0

## Component: enr-p2p debug capture

Operator-facing debug feature: a configurable on-disk ring buffer
that captures every frame transiting the P2P transport layer, in
pcap-compatible format. Disabled by default; opt-in via a
`[debug.p2p_capture]` section in `ergo.toml`.

The motivating use case: diagnosing peer-side wire-format bugs
without root-level packet capture. When a peer sends malformed
handshake bytes (or a new Rust implementation has serialization
bugs), the operator wants the exact bytes that hit our socket,
ready to load into Wireshark / tshark.

## SPECIAL Profile

```
S6  P5  E5  C7  I5  A7  L6
```

A7 — hot-path performance matters: every byte on the wire goes
through the capture hook when enabled. Per-frame cost must be
sub-100ns (atomic head + memcpy into mmap'd page).
C7 — operators reach for this when something's already broken;
the dump format and CLI ergonomics have to be obvious.
P5 — captures only bytes already crossing the network boundary;
nothing new from an adversarial perspective.
E5 — losing capture data during a crash is acceptable
(debug-only).

## Design Principles

- **Off by default.** Enabling carries disk + CPU cost. Operators
  opt in when they need it.
- **One global ring, not per-peer.** Per-peer rings add lifecycle
  questions (when is a ring created/destroyed? what happens on
  reconnect?) and routing CPU on the hot path. A single
  append-only ring with per-frame metadata (timestamp,
  direction, peer IP/port) is simpler in every dimension that
  matters.
- **File-backed via mmap, not heap.** Operators can size large
  rings (1 GB+) without RAM pressure. Survives process restart.
  Kernel handles dirty-page flushback on its own cadence;
  explicit `msync` only as an operator-tunable durability knob.
- **pcap-compatible output.** Wireshark / tshark / `tcpdump -r`
  / `scapy` work out of the box. No bespoke decoders required.
- **Capture raw, decode at dump time.** Hot path writes the
  unparsed frame bytes. Filtering, decoding, and analysis are
  cold-path work driven by operator dump requests.

## Configuration

```toml
[debug.p2p_capture]
enabled = true
path = "/var/lib/ergo-node/debug/p2p-capture.ring"   # REQUIRED when enabled
size_mb = 1024                                        # ring size in MiB
sync_interval_secs = 60                               # msync cadence; 0 = rely on kernel defaults
include_ips = []                                      # if non-empty, capture ONLY these
exclude_ips = ["192.168.1.100"]                       # if include_ips empty, drop these
```

`enabled = true` is the activation switch. Section absent
entirely → treated as disabled; no resources consumed, no hook
installed on the hot path.

### Required keys

`path` is REQUIRED when `enabled = true`. There is no built-in
default. The node exits non-zero at startup with one of three
clear diagnostics:

| Condition | Diagnostic |
|---|---|
| `enabled = true`, `path` not set | `p2p_capture.path is required when p2p_capture.enabled = true` |
| `path` set, parent directory does not exist | `p2p_capture.path parent directory does not exist: <dir>` |
| `path` set, parent exists but is not writable, OR file can't be created | `p2p_capture.path cannot be opened: <diagnostic>` |

The capture file's location is too operationally important to
silently default. A path on the wrong filesystem (e.g. a
tmpfs that disappears across reboot when the operator expected
persistence, or a small system partition that fills up under a 1
GB ring) is the kind of footgun this requirement is preventing.

All other keys have safe defaults: `size_mb = 1024`,
`sync_interval_secs = 60`, `include_ips = []`, `exclude_ips = []`.

`size_mb` is the pre-allocated ring size. Node calls
`fallocate(size_mb * 1MiB)` at startup. The file is created if
missing, truncated and re-initialized to the configured size if
present at a different size (capture data from a prior run is
lost on size change — operators are expected to dump before
resizing).

`sync_interval_secs` is the explicit `msync(MS_ASYNC)` cadence.
`0` means "let the kernel handle it" (default
`dirty_writeback_centisecs` is ~30s). Operators on slow disks
may want longer cadence; operators wanting tighter durability
may set ~10s.

### Filter semantics

`include_ips` and `exclude_ips` are exact-IP allowlists /
denylists. No CIDR, no port matching, no regex. Both are
parsed at startup into `HashSet<IpAddr>` (IPv4 entries
stored as IPv4 form, IPv6 entries as IPv6 form; no automatic
v4-mapped-v6 unification — exact match only).

| `include_ips` | `exclude_ips` | Behavior |
|---|---|---|
| empty | empty | Capture everything |
| non-empty | empty | Capture ONLY frames involving listed IPs |
| empty | non-empty | Drop frames involving listed IPs, capture rest |
| non-empty | non-empty | **Startup error.** Configuration is ambiguous; operator must choose one. |

"Involving" means: the peer at the other end of the connection
matches. The filter is applied to inbound + outbound frames
uniformly.

Per-frame filter lookup is O(1) (HashSet probe). For typical
list sizes (~10 IPs), cost is well under 100 ns.

## Storage format

The ring file is a pcap-format file with an Ergo-specific header
trailer at the END for ring-state metadata. Wireshark and other
pcap tools read the file from offset 0 and stop at the last
record — they don't see the trailer, so they read cleanly.

### File header (24 bytes, written once at file creation)

Standard pcap global header (`pcap_hdr_t`):

| Offset | Size | Field | Value |
|---|---|---|---|
| 0 | 4 | magic_number | `0xA1B2C3D4` (microsecond resolution) |
| 4 | 2 | version_major | `2` |
| 6 | 2 | version_minor | `4` |
| 8 | 4 | thiszone | `0` (UTC) |
| 12 | 4 | sigfigs | `0` |
| 16 | 4 | snaplen | configurable, default 2 MiB (max Ergo frame size) |
| 20 | 4 | network | `147` (LINKTYPE_USER0 — Ergo P2P frames carry their own framing, no link layer) |

### Per-record format

Each captured frame is preceded by a standard pcap-record header
(16 bytes), then 24 bytes of Ergo-specific metadata, then the raw
frame bytes:

```
+----------------+--------------------+----------------+
| pcap rec (16B) | ergo metadata (24B)| frame bytes (N)|
+----------------+--------------------+----------------+
```

**pcap record header**:

| Offset | Size | Field |
|---|---|---|
| 0 | 4 | ts_sec — Unix seconds |
| 4 | 4 | ts_usec — microseconds |
| 8 | 4 | incl_len — `24 + N` |
| 12 | 4 | orig_len — same as incl_len (no truncation in v1) |

**Ergo metadata block** (24 bytes — counted within `incl_len`):

| Offset | Size | Field |
|---|---|---|
| 0 | 1 | direction (`0` = inbound, `1` = outbound) |
| 1 | 1 | reserved (`0`) |
| 2 | 2 | peer_port (u16 big-endian) |
| 4 | 16 | peer_ip (IPv6 form; IPv4 mapped to `::ffff:a.b.c.d`) |
| 20 | 4 | reserved (`0`) |

**Frame bytes** — verbatim: magic (4) + code (1) + body length
(4 BE) + checksum (4) + body (N-13).

### Ring trailer (64 bytes, at end of file)

Pre-allocated; updated as captures advance:

| Offset | Size | Field |
|---|---|---|
| 0 | 8 | magic — ASCII "ENRPCAPR" |
| 8 | 4 | format_version (`1`) |
| 12 | 4 | reserved |
| 16 | 8 | write_head — byte offset within the data region for the next record |
| 24 | 8 | generation — increments on wrap |
| 32 | 32 | reserved |

The trailer lives at `file_size - 64`. Data region is from
offset 24 (after pcap header) to `file_size - 64`. Tools that
read pcap from offset 0 stop at the last valid record they can
parse and ignore the trailer.

## Ring mechanics

### Wrap policy

If writing a new record would cross the end of the data region,
the writer skips to offset 24 (right after the pcap header) and
writes there. The bytes between the last fully-written record
and the end of the region are left as `0xFF` padding (visible
in dumps as malformed packets that tools will skip).

Records are never split across the wrap boundary.

### Concurrent writes

The tap is installed directly at each peer connection's
frame-read and frame-write sites, so it is called concurrently
from N per-peer tasks. Serialization is via `Mutex<MmapMut>`
inside `RingBuffer::append` — the critical section is short
(claim slot, optional wrap-skip, memcpy, update trailer).

The `write_head` is held inside the mutex (not a free-standing
atomic) so wrap detection, padding write, and head reset are
naturally atomic with respect to other producers. A previous
draft proposed lockless `fetch_add` with CAS reset on wrap;
in practice the multi-producer install at frame I/O sites made
the mutex the simpler match for the call sites.

Generation counter increments on each wrap. Hot-path cost: one
mutex lock (uncontended in the common case, fast on tokio's
sync primitives), memcpy into the mapped region, brief trailer
update.

### Read-at-dump-time

Readers (operators issuing a `GET /debug/p2p-capture/dump`)
acquire a snapshot of `write_head` + `generation`, then walk
the ring from the oldest record. Oldest = immediately after
`write_head`, wrapping if needed. Newest = at `write_head - 1`.

The dump output is reordered chronologically (using the pcap
record `ts_sec` + `ts_usec` fields). Readers handle
mid-record bytes at the wrap boundary by recognizing the
`0xFF` padding and resuming at offset 24.

The dump endpoint streams the file to the client; for a 1 GB
ring, expect 10-30s download time over loopback.

## API endpoints (debug surface)

Loopback-only by default — share the bind with the rest of the
debug API.

### `GET /debug/p2p-capture/info`

Inspect ring state without dumping.

```json
{
  "enabled": true,
  "path": "/var/lib/ergo-node/debug/p2p-capture.ring",
  "size_mb": 1024,
  "write_head": 845321408,
  "generation": 3,
  "oldest_ts": "2026-05-25T11:23:45.123456Z",
  "newest_ts": "2026-05-25T13:55:12.654321Z",
  "filter_mode": "exclude",
  "filter_count": 1
}
```

### `GET /debug/p2p-capture/dump`

Stream the entire ring as a pcap file, chronologically ordered.

Response headers:
- `Content-Type: application/vnd.tcpdump.pcap`
- `Content-Disposition: attachment; filename="p2p-capture-{ISO_TIMESTAMP}.pcap"`

### `GET /debug/p2p-capture/dump?peer={ip}&since_secs={N}`

Filtered subset. Query params:

- `peer=<ip>` — only records where `peer_ip` matches. May be
  repeated for multiple peers.
- `since_secs=<N>` — only records with `ts_sec` within the
  last N seconds.
- `direction=<inbound|outbound>` — only records with this
  direction.

All filters are optional and combine with AND semantics. Same
content type and filename as the full dump.

### `POST /debug/p2p-capture/reset`

Reset `write_head` to `24` (start of data region) and
increment `generation`. Operator uses this before reproducing
a specific issue, so the post-reset capture is clean.

Response: `{"reset": true, "previous_generation": N, "current_generation": N+1}`

## Auto-dump triggers (v2 — not in initial release)

Forward-reference for design completeness; v1 ships without these.
Operators trigger dumps manually via the endpoints above.

Future additive features:

- **On peer disconnect with parse error / protocol violation**:
  auto-dump a window around the disconnect to
  `<path>.{timestamp}-{peer_ip}.pcap`. Window: configurable, default
  30s before + 5s after.
- **On process shutdown (SIGTERM)**: auto-dump the entire ring to
  `<path>.shutdown-{timestamp}.pcap` before exiting.

Both are additive; clients of the v1 contract do not need to
adopt them.

## Stability

| Aspect | Stability |
|---|---|
| `[debug.p2p_capture]` section name | Stable across major versions |
| Config keys (`path`, `size_mb`, etc.) | Stable across major versions; new optional keys added in minor |
| Ring file format (pcap header + per-record format + trailer) | Stable across minor versions; format changes bump major |
| Per-record Ergo metadata block | Stable; additions go at end of the block (24 bytes is fixed; rebumping requires major) |
| pcap LINKTYPE choice (USER0) | Stable; changing requires updating downstream dissectors |
| API endpoint paths | Stable across minor versions |
| `0xFF` wrap-padding semantics | Stable |

## Out of scope

- **Pre-handshake / pre-frame traffic**: capture is at the
  framing layer. TCP-level malformations that die before the
  first complete frame is delivered to the protocol layer are
  not captured. For those, `tcpdump`/`ss` are still the right
  tools.
- **TLS or encrypted P2P**: Ergo P2P doesn't use TLS. If it ever
  does, the hook captures decrypted bytes (post-decrypt). The
  contract sits at the application frame boundary, not the wire.
- **Frame body truncation**: v1 captures full frames including
  multi-MB block payloads. If ring-eviction-pressure becomes a
  concern, a `truncate_body_above_kb` setting can be added in a
  later additive minor version.
- **Online streaming** (`tail -f`-like): no streaming endpoint.
  Operators take snapshots; persistent streaming is a different
  feature.
- **Per-peer ring isolation**: explicitly rejected. One spammy
  peer can crowd out the bytes from a peer of interest;
  operators either size the ring large or use filters to scope
  what's captured.
- **CIDR / subnet filters**: exact IP only in v1. Operators
  who want to scope to a subnet list every member explicitly
  in `include_ips`. CIDR is an additive future minor.

## See also

- `facts/p2p-transport.md` — frame format and the framing layer
  where the capture hook is installed.
- `docs/protocol/ergo-p2p-wire-format.md` — full wire-format
  spec that dump output will be parsed against.
