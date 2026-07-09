# Observability

BootyCall emits **structured lifecycle events** through `tracing`. These are
plain log records on a dedicated target, `bootycall::events`, carrying typed
fields (MAC, architecture, byte counts, durations). They are meant to be
shipped into [ClickHouse](https://clickhouse.com) for querying boot activity;
the appliance itself has **no database dependency** and never connects out — it
just writes JSON lines. A collector on the host (or the systemd journal reader)
does the ingestion.

## Enabling JSON output

Logging format is chosen at startup by the `BOOTYCALL_LOG_FORMAT` environment
variable:

- `json` — one JSON object per line (events **and** logs).
- anything else, or unset — human-readable text (the default, for local dev).

The NixOS module sets `BOOTYCALL_LOG_FORMAT=json` on the systemd unit
automatically. `RUST_LOG` still applies as the level/target filter in both
modes.

Events are emitted at INFO level, so the default filter (`info`) includes them.

To keep the event stream while silencing routine INFO chatter from the rest of
the service, raise the global level and pin the event target back to INFO:

```text
RUST_LOG=warn,bootycall::events=info
```

The global `warn` quiets the per-request/subsystem INFO logs; the
`bootycall::events=info` directive keeps every record on the event target. Both
directives apply in text and JSON modes.

## Event catalogue

Every event record has `target = "bootycall::events"` and an `event` field
naming it. In the `tracing` JSON format the event name and payload live under a
`fields` object; `timestamp`, `level` and `target` are top-level.

- **`dhcp_pxe_offer`** — Proxy-DHCP answered a PXE client.
  Fields: `mac`, `arch`, `client_ip`, `bootloader`, `next_server`.
- **`tftp_transfer_complete`** — a bootloader finished sending over TFTP.
  Fields: `mac`, `file`, `bytes`, `blocks`, `windowsize`.
- **`tftp_transfer_error`** — TFTP could not open the requested file.
  Fields: `file`, `error`, `kind`.
- **`tftp_transfer_failed`** — a TFTP transfer aborted mid-flight.
  Fields: `mac`, `file`, `reason`.
- **`http_boot_served`** — HTTP served a host its boot/chainload script.
  Fields: `mac`, `target_mac`, `client_ip`.
- **`http_boot_render_failed`** — the boot template failed to render (HTTP 500).
  Fields: `mac`, `client_ip`.
- **`http_override_assigned`** — an operator assigned a manual boot target.
  Fields: `mac`, `target`.
- **`extract_cache_hit`** — a host's kernel/initrd cache was still valid.
  Fields: `host`, `mac`, `image`.
- **`extract_cache_miss`** — extraction started (cache stale or missing).
  Fields: `host`, `mac`, `image`.
- **`extract_complete`** — kernel and initrd extracted successfully.
  Fields: `host`, `mac`, `image`, `duration_ms`.
- **`extract_failed`** — extraction failed for a host.
  Fields: `host`, `mac`, `image`, `error`.
- **`extract_sync_all_failed`** — a cache sync in which every host failed.
  Fields: `hosts`, `failed`.
- **`extract_sync_partial`** — a cache sync in which some hosts failed.
  Fields: `hosts`, `succeeded`, `failed`.

The high-frequency HTTP poll loop (unmapped hosts re-poll every few seconds) is
intentionally **not** evented — first contact is already captured by
`dhcp_pxe_offer`, and per-poll records would swamp the stream. Add one later if
a heartbeat is genuinely wanted.

Example record (pretty-printed; on the wire it is one line):

```json
{
  "timestamp": "2026-07-08T12:34:56.789012Z",
  "level": "INFO",
  "target": "bootycall::events",
  "fields": {
    "event": "tftp_transfer_complete",
    "mac": "aa:bb:cc:dd:ee:ff",
    "file": "/var/lib/bootycall/tftpboot/boot/x64/ipxe.efi",
    "bytes": 1048576,
    "blocks": 2048,
    "windowsize": 16
  }
}
```

## Ingesting into ClickHouse

### 1. Table

A single wide table with the common fields plus a raw JSON column for anything
new keeps schema churn low:

```sql
CREATE TABLE bootycall_events
(
    timestamp    DateTime64(3),
    event        LowCardinality(String),
    mac          String,
    host         String,
    arch         LowCardinality(String),
    client_ip    String,
    file         String,
    image        String,
    target       String,
    target_mac   String,
    bytes        UInt64,
    blocks       UInt32,
    windowsize   UInt32,
    duration_ms  UInt32,
    error        String,
    raw          String
)
ENGINE = MergeTree
ORDER BY (event, timestamp);
```

Not every event field gets its own column — that is the point of `raw`. The
columns above are the curated, frequently-queried subset; the `flatten`
transform below writes the **whole** `fields` object to `raw`, so any field
without a column (`bootloader`, `next_server`, `kind`, `reason`, and the
`extract_sync_*` counts `hosts`/`succeeded`/`failed`) is still queryable with
`JSONExtractString(raw, 'next_server')` and friends. Add a column later only if
a field becomes hot.

### 2. Shipping the JSON lines

Point a collector at the service's stdout / journal and filter on the target.
With [Vector](https://vector.dev):

```toml
[sources.bootycall]
type = "journald"
include_units = ["bootycall.service"]

# Keep only the structured event records, drop human logs.
[transforms.events]
type = "filter"
inputs = ["bootycall"]
condition = '.target == "bootycall::events"'

# Lift the nested `fields` object to the top level for the table columns.
[transforms.flatten]
type = "remap"
inputs = ["events"]
source = '''
  . = merge(., object!(.fields) ?? {})
  .raw = encode_json(.fields)
'''

[sinks.clickhouse]
type = "clickhouse"
inputs = ["flatten"]
endpoint = "http://127.0.0.1:8123"
database = "default"
table = "bootycall_events"
skip_unknown_fields = true
```

If you prefer a pull-free file tap, log to a file and use
`clickhouse-client --query "INSERT INTO bootycall_events FORMAT JSONEachRow"`
fed by `tail -F`, applying the same `target` filter first.

### 3. Querying

```sql
-- Boot throughput over the last day
SELECT toStartOfHour(timestamp) AS hour, count() AS boots
FROM bootycall_events
WHERE event = 'tftp_transfer_complete' AND timestamp > now() - INTERVAL 1 DAY
GROUP BY hour ORDER BY hour;

-- Slowest extractions
SELECT host, image, duration_ms
FROM bootycall_events
WHERE event = 'extract_complete'
ORDER BY duration_ms DESC LIMIT 20;
```
