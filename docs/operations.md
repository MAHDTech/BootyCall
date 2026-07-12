# Operations Runbook

Day-two operations for a running BootyCall appliance: resetting a bad cache,
the hardware/`udev` requirements for the OLED and status LED, recovering a
wedged `systemd` unit, and where to find logs. For the event schema and
ClickHouse ingestion see [observability.md](observability.md); for first-time
setup see [usage.md](usage.md).

The examples assume the NixOS module (`services.bootycall`) with its defaults:
the unit is `bootycall.service`, state lives under `dataDir`
(`/var/lib/bootycall`), and the cache under `server.cacheDir`
(`/var/lib/bootycall/cache`). Adjust the paths if you overrode them.

## Where things live

| Path                          | What                                                                            |
| ----------------------------- | ------------------------------------------------------------------------------- |
| `/var/lib/bootycall`          | `dataDir` — service state (seeded `tftpboot/` and `static/` assets).            |
| `/var/lib/bootycall/cache`    | `server.cacheDir` — extracted kernel/initrd, one subdirectory per host.         |
| `/var/lib/bootycall/tftpboot` | `server.tftpRoot` — bootloaders and boot files served over TFTP.                |
| `bootycall.yaml`              | Active configuration (the module generates it, or points at your `configFile`). |
| `journalctl -u bootycall`     | Logs (structured JSON events plus human lines).                                 |
| `GET /api/health`             | Unauthenticated readiness probe (`200` healthy / `503` degraded).               |

Because the unit runs as a `DynamicUser`, files under `cacheDir`/`dataDir` are
owned by a per-boot dynamic UID. Operate on them as `root`.

## Resetting the cache after a bad extraction

BootyCall extracts each host's kernel and initrd out of its ISO/disk image into
a per-host cache directory:

```text
/var/lib/bootycall/cache/<mac>/
├── kernel
├── initrd
└── metadata.json   # image path, mtime, size, kernel/initrd overrides
```

`metadata.json` is the cache key. On every cache sync BootyCall compares it
against the current host config and re-extracts when the image (or an override)
has drifted. A sync runs **at service start and on every configuration reload**,
so you rarely need to touch the cache by hand.

You do need to intervene when an extraction was interrupted (e.g. the box lost
power mid-copy) and left a truncated `kernel`/`initrd` that still looks
"present" to the readiness check. To force a clean re-extraction:

1. Remove the affected host's cache (or the whole cache directory):

   ```bash
   # one host
   sudo rm -rf /var/lib/bootycall/cache/52:54:00:10:10:10
   # or start completely fresh
   sudo rm -rf /var/lib/bootycall/cache/*
   ```

2. Trigger a sync. Either edit the config file (the file watcher hot-reloads
   and re-syncs) or restart the unit:

   ```bash
   sudo systemctl restart bootycall
   ```

3. Confirm the box has recovered — `hosts_not_ready` should be empty:

   ```bash
   curl -fsS http://127.0.0.1:8080/api/health
   ```

Clearing the cache is always safe: anything removed is re-derived from the
source image on the next sync.

## Secure Deployment Defaults & Secrets

By default, BootyCall implements secure defaults:

- `services.bootycall.server.httpBind` defaults to `127.0.0.1:8080`, binding to loopback only.
- If `services.bootycall.openFirewall` is enabled and `httpBind` is set to a non-loopback address, you must define either `apiToken` or `apiTokenFile` to authenticate the mutating endpoints. An assertion prevents exposing the API unauthenticated on the network.

### API Authentication with apiTokenFile

To keep credentials out of the world-readable `/nix/store`, use `services.bootycall.server.apiTokenFile` to specify a path to the secret token:

```nix
services.bootycall = {
  enable = true;
  openFirewall = true;
  server.httpBind = "0.0.0.0:8080";
  server.apiTokenFile = "/run/secrets/bootycall-api-token";
};
```

At service start, the token is dynamically injected into a temporary configuration file `/run/bootycall/bootycall.yaml` which is restricted to the service user (permissions `0600` within `0700` directory).

### Preventing Denial of Service (maxArtifactBytes)

To guard the appliance against disk space exhaustion from huge or malformed extraction targets, set the maximum allowed size for any single extracted artifact:

```nix
services.bootycall.server.maxArtifactBytes = 1073741824; # 1 GiB limit
```

## Hardware: the OLED, status LED, and `udev`

The OLED display and status LED are off by default. The hardened unit runs with
`PrivateDevices = true`, which hides all of `/dev`. Enabling the accessory
relaxes that for the two device nodes it needs:

```nix
services.bootycall = {
  enable = true;
  hardware.enable = true; # DeviceAllow for gpiochip + fb0, joins gpio/video groups
};
```

`hardware.enable` swaps `PrivateDevices` for targeted `DeviceAllow` entries and
adds the `gpio` and `video` supplementary groups to the service's
`DynamicUser`. The framebuffer (`/dev/fb0`) already belongs to the standard
`video` group. The GPIO character device does **not** belong to a suitable
group by default, so the module also ships a `udev` rule assigning it to `gpio`
(controlled by `hardware.manageUdevRules`, default `true`):

```udev
SUBSYSTEM=="gpio", KERNEL=="gpiochip[0-9]*", GROUP="gpio", MODE="0660"
```

### Manual (non-module) deployments

If you run BootyCall outside the NixOS module, or set
`hardware.manageUdevRules = false` to own the rule yourself, create the group
and install the rule by hand:

```bash
sudo groupadd -r gpio                       # if it does not already exist
sudo tee /etc/udev/rules.d/99-bootycall-gpio.rules <<'EOF'
SUBSYSTEM=="gpio", KERNEL=="gpiochip[0-9]*", GROUP="gpio", MODE="0660"
EOF
sudo udevadm control --reload
sudo udevadm trigger --subsystem-match=gpio
```

Make sure the account BootyCall runs as is a member of the `gpio` group.

### Verifying device access

```bash
# The gpiochip node should be group-owned by gpio with mode 0660.
ls -l /dev/gpiochip0
# The unit should have DeviceAllow entries and the supplementary groups.
systemctl show bootycall -p DeviceAllow -p SupplementaryGroups
```

If the OLED stays blank, check the journal for a permission error opening
`/dev/gpiochip0` or `/dev/fb0` — that almost always means the `udev` rule did
not apply (device still `root:root`) or the process is not in the `gpio` group.

## Recovering a wedged unit

The unit runs with `Restart = always`, so a crash normally self-heals. When it
does not:

```bash
# 1. What state is it in, and why?
systemctl status bootycall
journalctl -u bootycall -e         # recent logs; add -f to follow

# 2. If it is stuck in a failed/restart loop, clear the failure and restart.
systemctl reset-failed bootycall
systemctl restart bootycall

# 3. Confirm readiness once it is back.
curl -fsS http://127.0.0.1:8080/api/health
```

A restart loop is usually a _data_ problem, not a config one — bad configs are
rejected at load and the previous good config keeps serving. The common cause is
that every host failed extraction (a missing or unreadable source image); the
journal logs this prominently and `/api/health` reports `degraded`. Fix the
image (or remove the host) and the next sync recovers. Note the process
deliberately does **not** exit on an all-hosts-failed sync, so it will not
restart-spin on a condition a restart cannot fix.

If a configuration edit did not take effect, confirm the file watcher saw it:
the log emits `Configuration file changed, reloading...` on a successful reload.
An invalid edit logs `Failed to reload configuration` and is ignored.

## Logs and the event stream

The unit sets `BOOTYCALL_LOG_FORMAT=json`, so lifecycle events are emitted as
one JSON object per line to stdout and captured by the journal:

```bash
# All logs.
journalctl -u bootycall -f
# Just the structured events, pretty-printed.
journalctl -u bootycall -o cat | jq -c 'select(.target == "bootycall::events")'
```

The most recent events are also available over HTTP at `GET /api/logs` (gated by
`server.apiToken` or `server.apiTokenFile` when configured). For the full event schema, field
reference, and how to ship these lines into ClickHouse, see
[observability.md](observability.md).
