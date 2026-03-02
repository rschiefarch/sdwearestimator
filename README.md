

A lightweight Linux daemon written in Rust that estimates flash wear on SD cards by monitoring kernel I/O statistics and modelling write amplification over time.

Designed primarily for always-on embedded Linux systems such as the **Raspberry Pi**, where SD card longevity is a common concern.

## Tips to Minimise SD Card Wear

For Raspberry Pi and other SBCs that mount root on an SD card, consider the following (they can potentially extend the life of your sd card by years):

### Tools
- **log2ram** — mounts `/var/log` in RAM and only flushes to disk periodically, dramatically reducing log write wear
- **tmpfs** — mount frequently written directories (e.g. `/tmp`, `/var/tmp`) in RAM so they never touch the SD card

### fstab Mount Options

Add the following options to your `/etc/fstab` entries for SD card mounted filesystems:

```
/dev/xxx  /mnt/yyy  ext4  defaults,noatime,nodiratime,commit=600,lazytime  0  2
```

| Option | Effect |
|---|---|
| `noatime` | Disables updating of file access timestamps on every read |
| `nodiratime` | Disables updating of directory access timestamps on every read |
| `commit=600` | Batches and caches write-through to disk every 10 minutes rather than the default 1 minute |
| `lazytime` | Delays flushing of time metadata (atime, mtime, ctime) to disk until the file is actually modified |


---

## How It Works

SD cards use NAND flash memory which has a finite number of **Program/Erase (P/E) cycles** before it wears out. The actual number of bytes written to the flash is always higher than what the host OS sends — this is called **Write Amplification (WAF)** and is caused by the SD card's internal Flash Translation Layer (FTL) having to erase and rewrite entire blocks even for small updates.

`sdestimator` monitors the Linux kernel's block I/O counters for your SD card device and:

1. Computes the **delta** of sectors and IOs written each poll interval
2. Estimates the **Write Amplification Factor** based on average IO size vs flash erase block size
3. Applies a **fullness multiplier** — a fuller card means more garbage collection pressure and higher WAF
4. Accumulates **estimated flash bytes written** over time
5. Derives the **average P/E cycle count** across the card and the **remaining life percentage**
6. Maintains a **rolling 28-day wear rate ring buffer** (1 sample per day, every 24 hours) to extrapolate how many years of write life remain at the current rate of use
7. Persists all state to a JSON file so tracking survives reboots and daemon restarts

It does **not** require any special kernel modules, hardware access, or SD card vendor tools. Everything is derived from standard Linux sysfs and procfs interfaces.

---

## Features

- 🔍 **Auto-detects card size** from `/sys/block/<dev>/size`
- 🔍 **Auto-detects erase block size** from `/sys/block/<dev>/queue/`
- 🔍 **Auto-detects mount points** by cross-referencing `/sys/block/` partitions with `/proc/mounts`
- 📊 **Fullness-aware WAF estimation** — accounts for GC pressure as the card fills up
- 📈 **Years-left extrapolation** — rolling 28-day wear rate used to estimate remaining write life in years
- 💾 **Persistent state** across reboots via a human-readable JSON file
- ⚙️ **systemd compatible** — designed to run as a background service
- 🦀 **Single binary**, no runtime dependencies

---

## Choosing a P/E Cycle Endurance Value for consumer series Sandisk SD Cards

> **Note:** The wear reduction tips at the top of this document can make a massive difference to the actual rate of wear observed in practice — a well-configured system may extends the life of the card by multiples of the default filesystem settings.

A large-scale microSD endurance survey tested 3 SanDisk Ultra 32GB cards until failure.
All three failed after an average of 3,142 read/write cycles.
One card failed as early as 2,747 cycles, showing inconsistency even within the same batch.
For context: a 32GB card enduring 3,142 cycles equates to writing ~100TB of data before failure.

From these tests, **2000** could be considered a conservative estimate for `--pe-cycles` when using SanDisk Ultra series cards.

See the full test results here: [The Great MicroSD Card Survey — Two Years Later](https://www.bahjeez.com/the-great-microsd-card-survey-two-years-later/)

---

## Installation

### ~~Pre-built Binary (Raspberry Pi 4) - don't download a binary just build from source~~

~~Download the latest release binary for `aarch64` from the [Releases](../../releases) page and copy it to your Pi:~~

```bash
scp sdestimator pi@<pi-ip>:/usr/local/bin/
ssh pi@<pi-ip> chmod +x /usr/local/bin/sdestimator
```

### Build From Source

You will need the [Rust toolchain](https://rustup.rs/) installed.

#### Native build (if building on the Pi itself)
```bash
cargo build --release
```

#### Cross-compile for Raspberry Pi 4 (aarch64) from Linux x86_64

Add the target and cross-linker:
```bash
rustup target add aarch64-unknown-linux-gnu
sudo apt install gcc-aarch64-linux-gnu
```

Build:
```bash
cargo build --release --target aarch64-unknown-linux-gnu
```

The binary will be at:
```
target/aarch64-unknown-linux-gnu/release/sdestimator
```

#### Using `cross` (easiest, requires Docker)
```bash
cargo install cross
cross build --release --target aarch64-unknown-linux-gnu
```

---

## Usage

```
sdestimator [OPTIONS]

Options:
  -d, --device <DEVICE>              Block device name [default: mmcblk0]
  -e, --erase-block-kb <KB>          Override erase block size in KB (auto-detected if not set)
  -p, --pe-cycles <N>                Rated P/E cycle endurance [default: 3000]
  -i, --interval <SECS>              Poll interval in seconds [default: 5]
      --state-file <PATH>            State file path [default: /var/lib/sdwear/state.json]
      --save-interval <SECS>         How often to save state to disk in seconds [default: 300]
      --initial-health <PERCENT>     Starting health percentage for a card already in use (0.0–100.0).
                                     Only applied when no existing state file is found; ignored if a
                                     state file already exists. Defaults to 100.0 (brand new card).
  -h, --help                         Print help
  -V, --version                      Print version
```

### Basic usage
```bash
sdestimator
```

### Custom P/E cycle rating (e.g. cheap TLC card rated at 1000 cycles)
```bash
sdestimator --pe-cycles 1000
```

### More frequent state saves
```bash
sdestimator --save-interval 60
```

### Different device
```bash
sdestimator --device mmcblk1
```

### Card already in use — preset starting health at 70%
```bash
sdestimator --initial-health 70
```

This is useful when you begin monitoring a card that has already been in service for some time.
The daemon will start tracking wear from a baseline of 70% life remaining rather than assuming
the card is brand new. If a state file already exists this flag is silently ignored so that
accumulated wear data is never overwritten.

---

## Output

On startup the daemon prints a summary of detected hardware and restored state:

```
sdwear — SD Card Flash Wear Estimator
Device:        /dev/mmcblk0
Card size:     128.0 GB (auto-detected)
Erase block:   4096 KB (auto-detected)
Rated P/E:     3000 cycles
State file:    /var/lib/sdwear/state.json
Save interval: 300 seconds
Mount point:   /dev/mmcblk0p1 → /boot
Mount point:   /dev/mmcblk0p2 → /
Lifetime budget: 384.0 TB total flash writes
─────────────────────────────────────────────────────────────
Restored state: 0.000012 avg P/E cycles, 99.9988% life remaining
─────────────────────────────────────────────────────────────
```

Each time write activity is detected during a poll interval, a labeled key=value line is printed:

```
ios=12 kb=480 avg_kb=40.0 waf=1.23 full=42.3% pe=0.000013 life=99.9987% yrs_left=4.5y
ios=8  kb=320 avg_kb=40.0 waf=1.23 full=42.4% pe=0.000014 life=99.9986% yrs_left=4.5y
```

The key=value format means each line is fully self-describing in the systemd journal even when
the startup header has long since scrolled out of view.

| Field | Description |
|---|---|
| `ios` | Number of write IOs observed this poll interval |
| `kb` | Kilobytes written to the device this poll interval (host side) |
| `avg_kb` | Average IO size in KB this poll interval |
| `waf` | Estimated Write Amplification Factor this poll interval |
| `full` | Filesystem fullness % (worst partition) — drives the WAF multiplier |
| `pe` | Cumulative estimated average P/E cycles consumed across the card |
| `life` | Estimated remaining life as % of rated P/E endurance |
| `yrs_left` | Extrapolated years of write life remaining at the current rolling wear rate. Shows `n/a` for the first 24 hours (not enough history yet), and `>100y` when the rate is so low the card is effectively not wearing out. |

---

## State File

State is persisted to a human-readable JSON file (default `/var/lib/sdwear/state.json`). This file is designed to be self-describing — you can read it at any time to check card health without running any tools.

The ring buffer of wear samples is also persisted here, so the rolling rate estimate and `yrs_left`
extrapolation survive daemon restarts and reboots without losing history.

```json
{
  "device": "mmcblk0",
  "card_size_gb": 128.0,
  "card_size_bytes": 137438953472,
  "erase_block_kb": 4096,
  "pe_cycles_rated": 3000,
  "mount_points": [
    "/dev/mmcblk0p1 -> /boot",
    "/dev/mmcblk0p2 -> /"
  ],
  "filesystem_fullness_pct": 42.3,
  "total_host_sectors_written": 1234567,
  "total_host_write_ios": 98765,
  "estimated_flash_bytes_written": 987654321,
  "estimated_avg_pe_cycles": 0.000123,
  "estimated_life_remaining_pct": 99.9877,
  "initial_health_pct": 100.0,
  "last_kernel_write_sectors": 1234567,
  "last_kernel_write_ios": 98765,
  "first_started": "1709123456",
  "last_updated": "1709456789",
  "reboot_count": 2,
  "wear_samples": [
    { "timestamp_secs": 1709100000, "flash_bytes_written": 900000000 },
    { "timestamp_secs": 1709186400, "flash_bytes_written": 940000000 },
    { "timestamp_secs": 1709272800, "flash_bytes_written": 987654321 }
  ],
  "last_sample_timestamp": 1709272800
}
```

The key fields for card health are:

| Field | Description |
|---|---|
| `estimated_avg_pe_cycles` | Average P/E cycles consumed per erase block across the whole card |
| `estimated_life_remaining_pct` | Estimated remaining life as % of rated endurance |
| `initial_health_pct` | Health % that monitoring started at (100.0 = brand new; lower if `--initial-health` was used) |
| `filesystem_fullness_pct` | How full the card was at last save — affects WAF |
| `reboot_count` | Number of reboots detected since monitoring began |
| `wear_samples` | Rolling ring buffer of 24-hourly wear snapshots (max 28 entries = 28 days) used to compute the `yrs_left` extrapolation |
| `last_sample_timestamp` | Unix timestamp of the most recent wear sample push |

---

## Running as a systemd Service

Create `/etc/systemd/system/sdwear.service`:

```ini
[Unit]
Description=SD Card Flash Wear Estimator
After=local-fs.target
Wants=local-fs.target

[Service]
Type=simple
ExecStartPre=/bin/mkdir -p /var/lib/sdwear
ExecStart=/usr/local/bin/sdestimator --pe-cycles 1500 --interval 5 --save-interval 900
Restart=on-failure
RestartSec=10
StandardOutput=journal
StandardError=journal
User=root

[Install]
WantedBy=multi-user.target
```

Then enable and start it:

```bash
sudo systemctl daemon-reload
sudo systemctl enable sdwear
sudo systemctl start sdwear
```

View live output:
```bash
journalctl -u sdwear -f
```

---

## Write Amplification Model

The WAF estimate is a two-stage heuristic:

### Stage 1 — IO size vs erase block size
| Average IO size | Base WAF |
|---|---|
| ≥ erase block size | 1.05 (nearly sequential) |
| Smaller | `1 + (erase_block / avg_io - 1) × 0.15` |

The `0.15` efficiency factor models the FTL's log-structured write buffering — real FTLs are significantly better than naive worst-case.

### Stage 2 — Fullness multiplier
| Card fullness | Multiplier |
|---|---|
| < 50% | 1.0× |
| 50–70% | 1.1× |
| 70–85% | 1.3× |
| 85–95% | 1.6× |
| > 95% | 2.0× |

A fuller card means the FTL has less room to manoeuvre, forcing more frequent and less efficient garbage collection.

---

## Years Remaining Extrapolation

The `yrs_left` value is derived from a **rolling 28-day wear rate ring buffer**. Every 24 hours a snapshot of `(timestamp, cumulative_flash_bytes_written)` is saved to the state file. Up to 28 snapshots are retained (1 per day × 28 days), adding only ~1.8 KB to the state file at full capacity.

To compute `yrs_left`:
1. Take the oldest snapshot in the buffer as the start of the window
2. Compute the wear rate: `bytes_written_in_window / elapsed_seconds`
3. Compute remaining flash capacity: `(life_remaining_pct / 100) × pe_cycles × card_bytes`
4. Extrapolate: `remaining_capacity / wear_rate / seconds_per_year`

| Display value | Meaning |
|---|---|
| `n/a` | Fewer than 2 samples in the buffer — need at least 24 hours of history |
| `4.5y` | Estimated years remaining at current rolling wear rate |
| `>100y` | Rate is so low the card is effectively not wearing out at current usage |

The estimate naturally adapts as usage patterns change — a period of heavy writes will pull the estimate down, and a quieter period will let it recover upward. The 28-day window strikes a balance between responsiveness and stability.

---

## Limitations and Caveats

- **Estimates only** — the SD card's FTL is a black box. The actual WAF depends on the card's internal firmware which is proprietary and varies between manufacturers and models.
- **P/E cycle rating** must be set manually via `--pe-cycles`. Consumer SD cards rarely publish this figure. TLC NAND is typically rated at 1,000–3,000 cycles.
- **Kernel counters only** — `sdestimator` sees what the host OS sends to the device, not what the device actually does internally.
- **Not a substitute for backups** — use this tool to inform your maintenance schedule, not as a guarantee of remaining life.

---

## Recommended P/E Cycle Values

| Card type | NAND type | Typical P/E cycles |
|---|---|---|
| Budget consumer SD | TLC | 500–1,000 |
| Mid-range consumer SD | TLC 3D NAND | 1,000–3,000 |
| Industrial / endurance SD | MLC | 3,000–10,000 |
| Industrial / endurance SD | SLC | 50,000–100,000 |

When in doubt, use `--pe-cycles 1000` for a conservative estimate on a typical consumer card.

---

## License

unlicense - see https://unlicense.org

---
