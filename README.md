

A lightweight Linux daemon written in Rust that estimates flash wear on SD cards by monitoring kernel I/O statistics and modelling write amplification over time.

Designed primarily for always-on embedded Linux systems such as the **Raspberry Pi**, where SD card longevity is a common concern.

To minimise sd card wear on Raspberry pi and sbcs that mount root on an sd card - consider the use of:
log2ram
tmpfs

extra fstab params:
/dev/xxx  /mnt/yyy  ext4  defaults,noatime,nodiratime,commit=600,lazytime  0  2
noatime doesnt update access time for files
nodiractime doesnt update accesstime for dirs
commit caches and batches writethrough for 10 minute rather than 1 minute
lazytime 
---

## How It Works

SD cards use NAND flash memory which has a finite number of **Program/Erase (P/E) cycles** before it wears out. The actual number of bytes written to the flash is always higher than what the host OS sends — this is called **Write Amplification (WAF)** and is caused by the SD card's internal Flash Translation Layer (FTL) having to erase and rewrite entire blocks even for small updates.

`sdestimator` monitors the Linux kernel's block I/O counters for your SD card device and:

1. Computes the **delta** of sectors and IOs written each poll interval
2. Estimates the **Write Amplification Factor** based on average IO size vs flash erase block size
3. Applies a **fullness multiplier** — a fuller card means more garbage collection pressure and higher WAF
4. Accumulates **estimated flash bytes written** over time
5. Derives the **average P/E cycle count** across the card and the **remaining life percentage**
6. Persists all state to a JSON file so tracking survives reboots

It does **not** require any special kernel modules, hardware access, or SD card vendor tools. Everything is derived from standard Linux sysfs and procfs interfaces.

---

## Features

- 🔍 **Auto-detects card size** from `/sys/block/<dev>/size`
- 🔍 **Auto-detects erase block size** from `/sys/block/<dev>/queue/`
- 🔍 **Auto-detects mount points** by cross-referencing `/sys/block/` partitions with `/proc/mounts`
- 📊 **Fullness-aware WAF estimation** — accounts for GC pressure as the card fills up
- 💾 **Persistent state** across reboots via a human-readable JSON file
- ⚙️ **systemd compatible** — designed to run as a background service
- 🦀 **Single binary**, no runtime dependencies

---

## Installation

### Pre-built Binary (Raspberry Pi 4)

Download the latest release binary for `aarch64` from the [Releases](../../releases) page and copy it to your Pi:

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
      Δ IOs       Δ KB   Avg KB    WAF   Full%      Avg P/E   Life %
         12        480     40.0   1.23   42.3%     0.000013  99.9987%
```

Each row is printed when write activity is detected during a poll interval.

---

## State File

State is persisted to a human-readable JSON file (default `/var/lib/sdwear/state.json`). This file is designed to be self-describing — you can read it at any time to check card health without running any tools:

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
  "last_kernel_write_sectors": 1234567,
  "last_kernel_write_ios": 98765,
  "first_started": "1709123456",
  "last_updated": "1709456789",
  "reboot_count": 2
}
```

The key fields for card health are:

| Field | Description |
|---|---|
| `estimated_avg_pe_cycles` | Average P/E cycles consumed per erase block across the whole card |
| `estimated_life_remaining_pct` | Estimated remaining life as % of rated endurance |
| `filesystem_fullness_pct` | How full the card was at last save — affects WAF |
| `reboot_count` | Number of reboots detected since monitoring began |

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
ExecStart=/usr/local/bin/sdestimator --pe-cycles 1500 --interval 5 --save-interval 300
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

unlicense - see https://unlicense.org>

---
