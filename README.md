

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

| Option | Effect                                                                                                                                                                              |
|---|-------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| `noatime` | Disables updating of file access timestamps on every read                                                                                                                           |
| `nodiratime` | Disables updating of directory access timestamps on every read                                                                                                                      |
| `commit=600` | (optional if you can handle more data loss from power outage - good if raspi on battery) Batches and caches write-through to disk every 10 minutes rather than the default 1 minute |
| `lazytime` | Delays flushing of time metadata (atime, mtime, ctime) to disk until the file is actually modified                                                                                  |


---

## How It Works

SD cards use NAND flash memory which has a finite number of **Program/Erase (P/E) cycles** before it wears out. The actual number of bytes written to the flash is always higher than what the host OS sends — this is called **Write Amplification (WAF)** and is caused by the SD card's internal Flash Translation Layer (FTL) having to manage page-level writes and periodic garbage collection.

`sdestimator` monitors the Linux kernel's block I/O counters for your SD card device and:

1. Computes the **delta** of sectors, IOs, and merged IOs written each poll interval
2. Estimates the **Write Amplification Factor** using a two-component model based on flash page size, sequential write ratio, and filesystem fullness
3. Accumulates **estimated flash bytes written** over time
4. Derives the **average P/E cycle count** across the card and the **remaining life percentage**
5. Maintains a **rolling 28-day wear rate ring buffer** (1 sample per day, every 24 hours) to extrapolate how many years of write life remain at the current rate of use
6. Persists all state to a JSON file so tracking survives reboots and daemon restarts

It does **not** require any special kernel modules, hardware access, or SD card vendor tools. Everything is derived from standard Linux sysfs and procfs interfaces.

---

## Features

- 🔍 **Auto-detects card size** from `/sys/block/<dev>/size`
- 🔍 **Auto-detects erase block size** from `/sys/block/<dev>/queue/`
- 🔍 **Auto-detects mount points** by cross-referencing `/sys/block/` partitions with `/proc/mounts`
- 📊 **Physics-based WAF estimation** — page-size + greedy GC model with dynamic sequential ratio from kernel merge stats
- 📈 **Years-left extrapolation** — rolling 28-day wear rate used to estimate remaining write life in years
- 💾 **Persistent state** across reboots via a human-readable JSON file
- ⚙️ **systemd compatible** — designed to run as a background service
- 🦀 **Single binary**, no runtime dependencies
- 🪶 **Tiny resident footprint** — musl static build runs at ~668 KB RSS on Raspberry Pi 4

---

## Choosing a P/E Cycle Endurance Value for Brands / Series of SD Cards

> **Note:** The wear reduction tips at the top of this document can make a massive difference to the actual rate of wear observed in practice — a well-configured system may extends the life of the card by multiples compared to the default settings.

A large-scale microSD endurance survey tested 3 SanDisk Ultra 32GB cards until failure.
All three failed after an average of 3,142 read/write cycles.
One card failed as early as 2,747 cycles, showing inconsistency even within the same batch.
For context: a 32GB card enduring 3,142 cycles equates to writing ~100TB of data before failure.

From these tests, **2000** could be considered a conservative estimate for `--pe-cycles` when using SanDisk Ultra series cards.

See the full test results here for other brands/series to get a pe-cycles endurance number:

[The Great MicroSD Card Survey — Two Years Later](https://www.bahjeez.com/the-great-microsd-card-survey-two-years-later/)

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

#### Cross-compile for Raspberry Pi 4 (aarch64) — glibc build
```bash
rustup target add aarch64-unknown-linux-gnu
sudo apt install gcc-aarch64-linux-gnu
cargo build --release --target aarch64-unknown-linux-gnu
```

The binary will be at:
```
target/aarch64-unknown-linux-gnu/release/sdestimator
```

#### Cross-compile for Raspberry Pi 4 (aarch64) — musl static build ✅ recommended

The musl build statically links the C runtime, eliminating the glibc runtime overhead entirely.
This results in a significantly smaller resident memory footprint (~668 KB RSS observed on Raspberry Pi 4
vs ~3 MB for a typical glibc-linked build). It also produces a fully self-contained binary with no
shared library dependencies, making deployment simpler.

```bash
rustup target add aarch64-unknown-linux-musl
cargo build --release --target aarch64-unknown-linux-musl
```

> **Note:** Unlike the glibc cross-compile, no external cross-linker package is required — the Rust
> toolchain handles everything via its bundled musl linker for this target.

The binary will be at:
```
target/aarch64-unknown-linux-musl/release/sdestimator
```

Copy to the Pi:
```bash
scp target/aarch64-unknown-linux-musl/release/sdestimator pi@<pi-ip>:/usr/local/bin/
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
      --flash-page-size <BYTES>      Flash page size in bytes used for WAF estimation [default: 16384]
      --over-provision <RATIO>       SD card over-provisioning ratio 0.0–0.5 [default: 0.07]
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

### Tuning the WAF model for your card
```bash
sdestimator --flash-page-size 4096 --over-provision 0.10
```

Use a smaller `--flash-page-size` for older or budget cards (4 KB is common), and a larger value
for modern high-density cards (16 KB or 32 KB). Use `--over-provision` to reflect the card's
reserved space — industrial cards often over-provision more aggressively (10–28%).

---

## Output

On startup the daemon prints a summary of detected hardware and restored state:

```
sdwear — SD Card Flash Wear Estimator
Device:        /dev/mmcblk0
Card size:     128.0 GB (auto-detected)
Erase block:   4096 KB (auto-detected)
Flash page:    16384 bytes
Over-prov:     7%
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
| `full` | Filesystem fullness % (worst partition) |
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

The WAF estimate uses a two-component physics-based model that is significantly more accurate than a naive erase-block ratio approach.

### Component 1 — Page-level WAF

The FTL programs data at **page granularity** (typically 4–16 KB), not at erase block granularity. Small writes that don't fill a full page cause the FTL to partially fill a page, wasting the remainder:

```
page_waf = max(1.0, flash_page_size / avg_io_bytes)
```

This is then weighted by a **sequential ratio** derived dynamically from the kernel's `write_merges` counter. The kernel block layer merges adjacent IOs before dispatching — a high merge count is a reliable indicator of sequential write patterns. Sequential writes can be coalesced by the FTL into full pages with near-zero amplification, while random writes suffer the full page_waf penalty:

```
sequential_ratio = write_merges / (write_ios + write_merges)
random_waf       = page_waf × (1 - sequential_ratio) + 1.0 × sequential_ratio
```

### Component 2 — Garbage Collection WAF

GC cost depends on how full the flash is. The **greedy GC model** (Desnoyers 2012) gives a well-established lower bound on GC amplification. The filesystem fill ratio is first adjusted for the card's over-provisioned space (which is invisible to the filesystem but available to the FTL):

```
effective_fill = fill_ratio × (1.0 - over_provision)
gc_waf         = 1.0 / (1.0 - effective_fill)
```

At 37% filesystem full with 7% over-provisioning: `effective_fill = 0.344`, `gc_waf ≈ 1.52`.

### Combination — additive not multiplicative

Multiplying `page_waf × gc_waf` would double-count: GC moves full pages so GC relocations don't suffer additional page-level amplification. The additive model is more physically defensible:

```
total_waf = random_waf + (gc_waf - 1.0)
```

### Behaviour across workloads

| Workload | Expected WAF |
|---|---|
| Large sequential writes at low fill | ≈ 1.0–1.5 |
| Mixed workload (50% sequential) at low fill | ≈ 2–4 |
| Small random writes at low fill (37%) | ≈ 3–5 |
| Small random writes at high fill (80%) | ≈ 6–10 |
| Small random writes at very high fill (95%) | ≈ 15–25 |

### Tuning parameters

| Parameter | CLI arg | Default | Notes |
|---|---|---|---|
| Flash page size | `--flash-page-size` | 16384 (16 KB) | Use 4096 for older/budget cards |
| Over-provisioning | `--over-provision` | 0.07 (7%) | Industrial cards often use 0.10–0.28 |

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
