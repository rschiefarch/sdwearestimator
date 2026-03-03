// src/main.rs
// altered by Poindexter AI: added --save-interval argument to replace hardcoded 60-tick save period
// altered by Poindexter AI: auto-detect card size, erase block size, and filesystem fullness for WAF multiplier
// altered by Poindexter AI: added human-readable descriptor fields to persisted state JSON
// altered by Poindexter AI: fix erase block zero bug; add LUKS/dm-crypt mount detection via /sys/block/<dev>/holders
// altered by Poindexter AI: add #[serde(default)] to all State fields for backwards compatibility with older state files
// altered by Poindexter AI: added --initial-health argument to preset starting life % for a card already in use
// altered by Poindexter AI: added 30-day rolling wear rate ring buffer and yrs_left extrapolation; switched to key=value output format
// altered by Poindexter AI: reduced wear sample ring buffer to 28 entries at 6-hour intervals (7 days, ~1.8 KB max)
// altered by Poindexter AI: changed ring buffer sample interval from 6 hours (21600s) to 24 hours (86400s) so 28 entries cover 28 days instead of 7
// altered by Poindexter AI: replaced erase-block-based WAF model with page-size + greedy-GC model with dynamic sequential ratio from kernel merge stats

use anyhow::{Context, Result};
use clap::Parser;
use nix::sys::statvfs::statvfs;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;
use std::thread;
use std::time::{Duration, Instant};

#[derive(Parser, Debug)]
#[command(version, about = "SD card flash wear estimator")]
struct Args {
    /// Block device name (e.g. mmcblk0)
    #[arg(short, long, default_value = "mmcblk0")]
    device: String,

    /// Override flash erase block size in KB. If not set, auto-detected from
    /// /sys/block/<dev>/queue/discard_granularity or optimal_io_size.
    #[arg(short, long)]
    erase_block_kb: Option<u64>,

    /// Flash page size in bytes. This is the granularity at which the FTL
    /// programs data — smaller than the erase block. Typical values are
    /// 4096 (4 KB) for older cards and 16384 (16 KB) for modern cards.
    /// Used as the base unit for WAF estimation.
    #[arg(long, default_value_t = 16384)]
    flash_page_size: u64,

    /// SD card over-provisioning ratio (0.0–1.0). Most consumer SD cards
    /// reserve ~7% of flash capacity for internal FTL use. This adjusts the
    /// effective fill level seen by the GC model so that filesystem fullness
    /// is correctly mapped to flash-level occupancy.
    #[arg(long, default_value_t = 0.07)]
    over_provision: f64,

    /// Rated P/E cycles (TLC≈1000-3000, MLC≈3000-10000)
    #[arg(short, long, default_value_t = 3000)]
    pe_cycles: u64,

    /// Poll interval in seconds
    #[arg(short, long, default_value_t = 5)]
    interval: u64,

    /// State file path for persistence across reboots
    #[arg(long, default_value = "/var/lib/sdwear/state.json")]
    state_file: PathBuf,

    /// How often to save state to disk in seconds (default: 300 = 5 minutes)
    #[arg(long, default_value_t = 300)]
    save_interval: u64,

    /// Preset the starting health percentage for a card you have already been using
    /// (0.0 – 100.0). Only applied when no existing state file is found; ignored
    /// if a state file already exists so accumulated wear data is never overwritten.
    /// Defaults to 100.0 (brand new card) if not specified.
    #[arg(long)]
    initial_health: Option<f64>,
}

// ─── Serde default helpers ───
//
// Serde's #[serde(default)] attribute uses the Default trait for primitive types
// (giving 0, false, empty string etc). For fields where the correct default is
// something other than zero we need a small helper function that serde can call.
//
// This is what makes the state file backwards compatible — if a field is missing
// from an older JSON file, serde will call the appropriate default function
// rather than failing the entire deserialisation.

/// Default life remaining is 100% (brand new card, no wear recorded)
fn default_life_remaining() -> f64 {
    100.0
}

/// Default initial health is 100% — used when reading an older state file that
/// predates the initial_health_pct field, meaning monitoring started on a fresh card.
fn default_initial_health() -> f64 {
    100.0
}

// ─── Wear sample for the rolling rate ring buffer ───
//
// Each sample records a unix timestamp (seconds) and the cumulative estimated
// flash bytes written at that point in time. Samples are pushed once every
// 24 hours (86400 seconds). The ring buffer holds at most WEAR_SAMPLE_MAX
// entries (28 = 28 days × 1 sample per day).
//
// To estimate the wear rate we look at the oldest sample in the buffer and
// compute bytes-per-second over the window it spans, then extrapolate to years.

/// Maximum number of wear samples retained in the ring buffer.
/// 1 sample/day × 28 days = 28 entries. At ~65 bytes per entry in pretty-printed
/// JSON this keeps the state file contribution from the buffer to ~1.8 KB max.
const WEAR_SAMPLE_MAX: usize = 28; // 28 days × 1 sample per day

/// Minimum seconds between wear samples (24 hours)
const WEAR_SAMPLE_INTERVAL_SECS: u64 = 86400;

#[derive(Serialize, Deserialize, Debug, Clone)]
struct WearSample {
    /// Unix timestamp (seconds since epoch) when this sample was taken
    timestamp_secs: u64,
    /// Cumulative estimated flash bytes written at the time of this sample
    flash_bytes_written: u64,
}

// ─── Persistent state: survives reboots ───
//
// This struct is serialised to JSON. It is designed to be human-readable
// so that an operator can open the file and immediately understand the
// health of the SD card without needing to know the program arguments
// that were used to start the daemon.
//
// Fields are grouped into four sections:
//   1. Descriptor fields  — what device/card this file relates to
//   2. Cumulative metrics — the wear tracking numbers
//   3. Internal counters  — used by the algorithm for delta/reboot detection
//   4. Rolling wear rate ring buffer — 24-hourly snapshots for yrs_left estimation
//
// Every field carries #[serde(default)] so that if a field is absent in an
// older state file (e.g. from a previous version of the binary that didn't
// have that field yet), deserialisation succeeds rather than failing and
// losing all accumulated wear history. This makes the state file format
// forwards and backwards compatible across binary upgrades.

#[derive(Serialize, Deserialize, Debug, Clone)]
struct State {

    // ── Section 1: Descriptor fields ──
    // These are refreshed on every save so the file is always self-describing.

    /// The block device being monitored (e.g. "mmcblk0")
    #[serde(default)]
    device: String,

    /// Card capacity in GB as reported by the kernel (human readable)
    #[serde(default)]
    card_size_gb: f64,

    /// Card capacity in exact bytes as reported by the kernel
    #[serde(default)]
    card_size_bytes: u64,

    /// Erase block size in KB (auto-detected or user override)
    #[serde(default)]
    erase_block_kb: u64,

    /// Rated P/E cycle endurance used for life% calculation
    #[serde(default)]
    pe_cycles_rated: u64,

    /// Mount points detected for this device's partitions at last save.
    /// Includes direct mounts and mounts via dm-crypt/LUKS device mapper volumes.
    #[serde(default)]
    mount_points: Vec<String>,

    /// Filesystem fullness percentage at last save (worst partition, drives WAF multiplier)
    #[serde(default)]
    filesystem_fullness_pct: f64,

    // ── Section 2: Cumulative wear metrics ──

    /// Cumulative 512-byte sectors written as seen from host side
    #[serde(default)]
    total_host_sectors_written: u64,

    /// Cumulative write IO count from host side
    #[serde(default)]
    total_host_write_ios: u64,

    /// Cumulative estimated bytes actually written to flash (after WAF adjustment)
    #[serde(default)]
    estimated_flash_bytes_written: u64,

    /// ★ THE KEY NUMBER ★
    /// Estimated average P/E cycle count per erase block across the whole card.
    /// Calculated as: estimated_flash_bytes_written / card_size_bytes
    /// When this approaches pe_cycles_rated the card is near end of life.
    #[serde(default)]
    estimated_avg_pe_cycles: f64,

    /// Estimated remaining life as a percentage of rated P/E endurance.
    /// 100.0 = brand new, 0.0 = end of rated life.
    /// Default is 100.0 (not 0.0) — a missing field means no wear recorded yet.
    #[serde(default = "default_life_remaining")]
    estimated_life_remaining_pct: f64,

    /// The health percentage that monitoring started at for this card.
    /// Set to the --initial-health argument value when first creating the state file,
    /// or 100.0 if monitoring started on a brand new card (or the field is absent
    /// in an older state file). Stored purely for reference — not used in calculations.
    #[serde(default = "default_initial_health")]
    initial_health_pct: f64,

    // ── Section 3: Internal algorithm counters ──

    /// Raw kernel write sector counter at last poll — used to compute deltas
    /// and detect reboots (kernel counters reset to 0 on boot)
    #[serde(default)]
    last_kernel_write_sectors: u64,

    /// Raw kernel write IO counter at last poll
    #[serde(default)]
    last_kernel_write_ios: u64,

    /// Unix timestamp when monitoring first started on this card
    #[serde(default)]
    first_started: String,

    /// Unix timestamp of the last state file save
    #[serde(default)]
    last_updated: String,

    /// Number of reboots detected since monitoring began
    #[serde(default)]
    reboot_count: u64,

    // ── Section 4: Rolling wear rate ring buffer ──
    //
    // Snapshots of (timestamp, cumulative_flash_bytes_written) taken every 24 hours.
    // Capped at WEAR_SAMPLE_MAX (28) entries = 28 days of history.
    // Used to compute a rolling wear rate for the yrs_left extrapolation.
    // Older entries are evicted from the front when the buffer is full.

    /// 24-hourly wear samples for rolling rate calculation (max 28 = 28 days)
    #[serde(default)]
    wear_samples: Vec<WearSample>,

    /// Unix timestamp of the last wear sample push
    #[serde(default)]
    last_sample_timestamp: u64,
}

impl State {
    /// Create a fresh state for a card that has never been monitored before.
    /// Descriptor fields are populated separately after detection in main().
    fn new() -> Self {
        let now = now_string();
        Self {
            // Descriptor fields — filled in after detection, zeroed here
            device: String::new(),
            card_size_gb: 0.0,
            card_size_bytes: 0,
            erase_block_kb: 0,
            pe_cycles_rated: 0,
            mount_points: vec![],
            filesystem_fullness_pct: 0.0,

            // Cumulative metrics
            total_host_sectors_written: 0,
            total_host_write_ios: 0,
            estimated_flash_bytes_written: 0,
            estimated_avg_pe_cycles: 0.0,
            estimated_life_remaining_pct: 100.0,

            // Initial health defaults to 100% (brand new card)
            initial_health_pct: 100.0,

            // Internal counters
            last_kernel_write_sectors: 0,
            last_kernel_write_ios: 0,
            first_started: now.clone(),
            last_updated: now,
            reboot_count: 0,

            // Rolling wear rate ring buffer — empty on first start
            wear_samples: Vec::new(),
            last_sample_timestamp: 0,
        }
    }
}

// ─── Reading /sys/block/<dev>/stat ───
//
// The kernel stat file has the following field layout (space separated):
//   0=read_ios  1=read_merges  2=read_sectors  3=read_ticks
//   4=write_ios 5=write_merges 6=write_sectors 7=write_ticks
//   8=in_flight 9=io_ticks    10=time_in_queue ...
//
// We read write_ios, write_merges, and write_sectors.
// write_merges counts how many adjacent write requests the kernel merged
// before dispatching to the device — a high merge count relative to write_ios
// indicates a more sequential write pattern, which we use to compute the
// sequential_ratio for the WAF model.

struct KernelStats {
    write_ios: u64,
    write_merges: u64,
    write_sectors: u64,
}

fn read_kernel_stats(device: &str) -> Result<KernelStats> {
    let path = format!("/sys/block/{}/stat", device);
    let content = fs::read_to_string(&path)
        .with_context(|| format!("Cannot read {path}"))?;

    let fields: Vec<u64> = content
        .split_whitespace()
        .filter_map(|s| s.parse().ok())
        .collect();

    // Field indices per kernel docs:
    //   0=read_ios 1=read_merges 2=read_sectors 3=read_ticks
    //   4=write_ios 5=write_merges 6=write_sectors 7=write_ticks ...
    Ok(KernelStats {
        write_ios:     *fields.get(4).unwrap_or(&0),
        write_merges:  *fields.get(5).unwrap_or(&0),
        write_sectors: *fields.get(6).unwrap_or(&0),
    })
}

// ─── Auto-detect card size ───
//
// /sys/block/<dev>/size contains the device size in 512-byte sectors.
// Multiply by 512 to get bytes.

fn detect_card_bytes(device: &str) -> Result<u64> {
    let path = format!("/sys/block/{}/size", device);
    let content = fs::read_to_string(&path)
        .with_context(|| format!("Cannot read card size from {path}"))?;
    let sectors: u64 = content.trim().parse()
        .with_context(|| format!("Cannot parse card size from {path}"))?;
    Ok(sectors * 512)
}

// ─── Auto-detect erase block size ───
//
// We try two sysfs queue attributes in order of preference:
//
//   1. discard_granularity — on cards supporting TRIM/discard this directly
//      reflects the erase block size the controller exposes
//   2. optimal_io_size — the controller's preferred IO alignment, often
//      matches the erase block size
//
// Both are in bytes. The value must be:
//   - Greater than zero
//   - A power of two
//   - At least 64 KB (values smaller than this are not realistic erase block
//     sizes and indicate the driver is reporting a logical rather than physical
//     geometry — USB devices in particular often report misleadingly small values)
//
// If neither attribute gives a sensible value we fall back to 4 MB (4096 KB)
// which is a reasonable default for modern consumer SD and USB flash devices.
//
// The user can always override with --erase-block-kb if they know better.
//
// NOTE: the erase block size is still used for the lifetime budget display
// and P/E cycle accounting. It is no longer used in the WAF calculation itself
// (which now uses flash_page_size instead).

fn detect_erase_block_bytes(device: &str) -> u64 {
    // Fallback default: 4 MB erase block
    let fallback: u64 = 4096 * 1024;

    // Minimum plausible erase block size — anything smaller is almost certainly
    // a logical block size being reported rather than a physical erase block size.
    // USB mass storage devices in particular often report 512 or 4096 bytes here.
    let min_plausible_bytes: u64 = 64 * 1024; // 64 KB

    // Helper: read a sysfs queue attribute as u64 bytes, applying sanity checks
    let read_queue_attr = |attr: &str| -> Option<u64> {
        let path = format!("/sys/block/{}/queue/{}", device, attr);
        fs::read_to_string(&path)
            .ok()
            .and_then(|s| s.trim().parse::<u64>().ok())
            // Must be non-zero, a power of two, and at least 64 KB to be plausible
            .filter(|&v| v >= min_plausible_bytes && v.is_power_of_two())
    };

    // Try discard_granularity first (most reliable for flash devices), then optimal_io_size
    read_queue_attr("discard_granularity")
        .or_else(|| read_queue_attr("optimal_io_size"))
        .unwrap_or(fallback)
}

// ─── Auto-detect mount points for this device ───
//
// This function handles three cases:
//
//   Case 1 — Direct mount: partition is mounted directly
//     e.g. /dev/mmcblk0p2 -> /
//
//   Case 2 — No partition table: raw device is mounted directly
//     e.g. /dev/sda -> /mnt/data
//
//   Case 3 — LUKS/dm-crypt: device is encrypted, the decrypted mapper
//     device is what gets mounted, not the raw device itself.
//     e.g. /dev/sda -> (LUKS) -> /dev/mapper/enc -> /mnt/enc
//
//     For case 3 we look in /sys/block/<dev>/holders/ which lists the
//     device mapper (dm-*) devices that sit on top of this block device.
//     We then read the dm device's name from /sys/block/dm-N/dm/name
//     to get the mapper name (e.g. "enc"), construct /dev/mapper/enc,
//     and check /proc/mounts for that.
//
// Returns a list of (mounted_device, mount_point) pairs.

fn find_mount_points(device: &str) -> Vec<(String, String)> {
    // Build the set of candidate device paths to look for in /proc/mounts.
    // Start with the raw device and all its direct partitions.
    let mut candidates: Vec<String> = vec![format!("/dev/{}", device)];

    // Scan /sys/block/<dev>/ for partition subdirectories
    let sys_path = format!("/sys/block/{}", device);
    if let Ok(entries) = fs::read_dir(&sys_path) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            // Partition directories start with the device name (e.g. mmcblk0p1, sda1)
            if name.starts_with(device) {
                candidates.push(format!("/dev/{}", name));
            }
        }
    }

    // ── Case 3: LUKS/dm-crypt holder detection ──
    //
    // Check /sys/block/<dev>/holders/ for device mapper children.
    // Each entry is a symlink to a dm-N device that sits on top of this device.
    // We resolve the dm device name via /sys/block/dm-N/dm/name to get the
    // mapper name, then add /dev/mapper/<name> as a candidate.
    let holders_path = format!("/sys/block/{}/holders", device);
    if let Ok(entries) = fs::read_dir(&holders_path) {
        for entry in entries.flatten() {
            let holder_name = entry.file_name().to_string_lossy().to_string();

            // holders/ entries are dm-N device names
            if holder_name.starts_with("dm-") {
                // Read the human-readable mapper name from the dm device's sysfs entry
                // e.g. /sys/block/dm-0/dm/name -> "enc"
                let dm_name_path = format!("/sys/block/{}/dm/name", holder_name);
                if let Ok(mapper_name) = fs::read_to_string(&dm_name_path) {
                    let mapper_name = mapper_name.trim();
                    if !mapper_name.is_empty() {
                        // Add /dev/mapper/<name> as a candidate mount device
                        candidates.push(format!("/dev/mapper/{}", mapper_name));
                    }
                }
            }
        }
    }

    // Parse /proc/mounts and find all entries matching our candidates.
    // Format: "<device> <mountpoint> <fstype> <options> 0 0"
    let mounts_content = match fs::read_to_string("/proc/mounts") {
        Ok(c) => c,
        Err(_) => return vec![],
    };

    let mut result = Vec::new();
    for line in mounts_content.lines() {
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.len() < 2 {
            continue;
        }
        let mount_dev = parts[0];
        let mount_point = parts[1];

        // Check if this mount entry matches one of our candidate device paths
        if candidates.iter().any(|c| c == mount_dev) {
            result.push((mount_dev.to_string(), mount_point.to_string()));
        }
    }

    result
}

// ─── Filesystem fullness ───
//
// For each mount point, use statvfs to determine how full the filesystem is.
// Returns the highest usage fraction across all mounted partitions of the device.
// We use the worst (most full) partition because GC pressure is a card-wide
// phenomenon — a nearly-full partition stresses the entire FTL.

fn max_filesystem_fullness(mount_points: &[(String, String)]) -> f64 {
    let mut max_used: f64 = 0.0;

    for (_, mount_point) in mount_points {
        // statvfs gives us block counts for the filesystem
        if let Ok(stat) = statvfs(mount_point.as_str()) {
            let total = stat.blocks() as f64 * stat.block_size() as f64;
            let free = stat.blocks_free() as f64 * stat.block_size() as f64;
            if total > 0.0 {
                let used_frac = 1.0 - (free / total);
                if used_frac > max_used {
                    max_used = used_frac;
                }
            }
        }
    }

    max_used
}

// ─── Write amplification estimation ───
//
// This model replaces the previous erase-block-based approach with a more
// physically accurate two-component model:
//
//   Component 1 — Page-level WAF (normal write amplification)
//   ──────────────────────────────────────────────────────────
//   The FTL programs data at page granularity (typically 4–16 KB), not at
//   erase block granularity. Small writes that don't fill a full page cause
//   the FTL to partially fill a page, wasting the remainder.
//
//     page_waf = max(1.0, flash_page_size / avg_io_bytes)
//
//   We then weight this by the sequential ratio — sequential writes can be
//   coalesced by the FTL into full pages with near-zero amplification, while
//   random writes suffer the full page_waf penalty:
//
//     random_waf = page_waf × (1 - sequential_ratio) + 1.0 × sequential_ratio
//
//   The sequential_ratio is derived from the kernel's write_merges counter:
//     sequential_ratio = write_merges / (write_ios + write_merges)
//   A high merge count means the kernel is coalescing adjacent IOs before
//   dispatching — a reliable signal of sequential write patterns.
//
//   Component 2 — Garbage collection WAF
//   ──────────────────────────────────────
//   GC cost depends on how full the flash is. We use the greedy GC model
//   (Desnoyers 2012) which gives a lower bound on GC amplification:
//
//     gc_waf = 1.0 / (1.0 - effective_fill)
//
//   where effective_fill adjusts for the card's over-provisioned space:
//
//     effective_fill = fill_ratio × (1.0 - over_provision)
//
//   At 37% filesystem full with 7% over-provisioning:
//     effective_fill = 0.37 × 0.93 = 0.344
//     gc_waf = 1 / (1 - 0.344) = 1.52
//
//   Combination — additive not multiplicative
//   ──────────────────────────────────────────
//   Multiplying page_waf × gc_waf double-counts: GC moves full pages so
//   GC relocations don't suffer additional page-level amplification.
//   The additive model is more physically defensible:
//
//     total_waf = random_waf + (gc_waf - 1.0)
//
//   This gives:
//     - Sequential writes at low fill: WAF ≈ 1 + small GC overhead ✓
//     - Random small writes at low fill: WAF ≈ 3–5 ✓
//     - Random small writes at high fill: WAF climbs steeply ✓
//     - Large writes: page_waf drops toward 1.0 ✓

fn estimate_waf(
    delta_sectors: u64,
    delta_ios: u64,
    delta_merges: u64,
    flash_page_bytes: u64,
    fill_ratio: f64,
    over_provision: f64,
) -> f64 {
    if delta_ios == 0 || delta_sectors == 0 {
        return 1.0;
    }

    // Guard against a zero page size — should not happen with validated args
    // but we defend here to avoid a divide-by-zero panic
    if flash_page_bytes == 0 {
        return 1.0;
    }

    // Average host IO size in bytes for this poll interval
    let avg_io_bytes = (delta_sectors * 512) as f64 / delta_ios as f64;

    // ── Component 1: Page-level WAF ──
    //
    // How many times larger is the flash page than the average IO?
    // Clamped to a minimum of 1.0 — writes larger than a page have no
    // page-level amplification.
    let page_waf = (flash_page_bytes as f64 / avg_io_bytes).max(1.0);

    // ── Sequential ratio from kernel merge stats ──
    //
    // write_merges counts IOs that the kernel block layer merged with an
    // adjacent IO before dispatching. A merged IO is a strong indicator
    // of sequential access — random IOs are almost never adjacent.
    //
    // sequential_ratio = merges / (dispatched_ios + merges)
    //
    // This gives a value in [0.0, 1.0]:
    //   0.0 = fully random (no merges at all)
    //   1.0 = fully sequential (all IOs were merged)
    let total_io_events = delta_ios + delta_merges;
    let sequential_ratio = if total_io_events > 0 {
        delta_merges as f64 / total_io_events as f64
    } else {
        0.0
    };

    // ── Blend page_waf by sequential ratio ──
    //
    // Sequential writes can be coalesced by the FTL into full pages with
    // near-zero amplification (WAF ≈ 1.0). Random writes suffer the full
    // page_waf penalty. We linearly interpolate between the two extremes.
    let random_waf = page_waf * (1.0 - sequential_ratio) + 1.0 * sequential_ratio;

    // ── Component 2: GC WAF using greedy GC model ──
    //
    // Adjust filesystem fill ratio for the card's over-provisioned space.
    // Over-provisioned space is invisible to the filesystem but available
    // to the FTL for GC staging, so the FTL sees a lower effective fill level.
    let effective_fill = fill_ratio * (1.0 - over_provision);

    // Greedy GC model: gc_waf = 1 / (1 - effective_fill)
    // Clamp effective_fill to [0.0, 0.99] to avoid division by zero or
    // astronomically large values when the card is nearly full.
    let effective_fill_clamped = effective_fill.clamp(0.0, 0.99);
    let gc_waf = 1.0 / (1.0 - effective_fill_clamped);

    // ── Additive combination ──
    //
    // total_waf = random_waf + (gc_waf - 1.0)
    //
    // The (gc_waf - 1.0) term represents the extra writes caused by GC
    // beyond the baseline 1:1 write. Adding it to random_waf rather than
    // multiplying avoids double-counting page-level amplification in GC paths.
    let total_waf = random_waf + (gc_waf - 1.0);

    // Ensure WAF is never below 1.0 (cannot write less than what was sent)
    total_waf.max(1.0)
}

// ─── Rolling wear rate ring buffer management ───
//
// We push one sample every WEAR_SAMPLE_INTERVAL_SECS (24 hours) into the ring
// buffer. When the buffer reaches WEAR_SAMPLE_MAX (28) entries the oldest entry
// is evicted from the front, keeping a rolling 28-day window.

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

/// Push a new wear sample if at least WEAR_SAMPLE_INTERVAL_SECS (24 hours) have
/// elapsed since the last sample. Evicts the oldest entry when the buffer is
/// full (28 entries = 28 days at 1 sample/day).
fn maybe_push_wear_sample(state: &mut State) {
    let now = now_secs();

    // Only push a sample if the configured interval has elapsed since the last one
    if now.saturating_sub(state.last_sample_timestamp) < WEAR_SAMPLE_INTERVAL_SECS {
        return;
    }

    // Evict the oldest sample if we are at capacity to keep the buffer bounded
    if state.wear_samples.len() >= WEAR_SAMPLE_MAX {
        state.wear_samples.remove(0);
    }

    // Push the current snapshot
    state.wear_samples.push(WearSample {
        timestamp_secs: now,
        flash_bytes_written: state.estimated_flash_bytes_written,
    });

    state.last_sample_timestamp = now;
}

// ─── Years-left extrapolation ───
//
// Uses the rolling wear rate ring buffer to estimate how many years of write
// life remain on the card at the current rate of wear.
//
// Algorithm:
//   1. Take the oldest sample in the ring buffer as the start of the window
//      (up to 28 days ago).
//   2. Compute bytes written and time elapsed over that window.
//   3. Derive a wear rate in bytes/second.
//   4. Compute remaining flash bytes capacity:
//        remaining_bytes = (life_remaining_pct / 100) * pe_cycles_rated * card_bytes
//   5. years_left = remaining_bytes / wear_rate / seconds_per_year
//
// Returns None if:
//   - The ring buffer has fewer than 2 samples (not enough data yet — need at
//     least 24 hours of history before the first estimate is produced)
//   - The time window is zero (shouldn't happen but guard anyway)
//   - The wear rate is zero (no writes have occurred in the window)
//
// Returns Some(f64) capped at 100.0 — values above 100 years are displayed
// as ">100y" since they are not meaningfully actionable.

fn estimate_years_left(state: &State, card_bytes: u64, pe_cycles: u64) -> Option<f64> {
    // Need at least two samples to compute a rate
    if state.wear_samples.len() < 2 {
        return None;
    }

    // The oldest sample defines the start of the rolling window (up to 28 days ago)
    let oldest = &state.wear_samples[0];
    let now_ts = now_secs();

    // Time elapsed over the window in seconds
    let elapsed_secs = now_ts.saturating_sub(oldest.timestamp_secs);
    if elapsed_secs == 0 {
        return None;
    }

    // Flash bytes written during the window
    let bytes_in_window = state.estimated_flash_bytes_written
        .saturating_sub(oldest.flash_bytes_written);

    // If no bytes were written in the window the rate is zero — cannot extrapolate
    if bytes_in_window == 0 {
        return None;
    }

    // Wear rate in bytes per second over the rolling window
    let bytes_per_sec = bytes_in_window as f64 / elapsed_secs as f64;

    // Total flash write capacity of the card:
    //   capacity = pe_cycles_rated * card_bytes
    // Remaining capacity based on current life percentage:
    //   remaining = (life_remaining_pct / 100.0) * capacity
    let remaining_flash_bytes =
        (state.estimated_life_remaining_pct / 100.0) * (pe_cycles as f64 * card_bytes as f64);

    // Seconds per year (365.25 days to account for leap years)
    let secs_per_year = 365.25 * 24.0 * 3600.0;

    let years = remaining_flash_bytes / bytes_per_sec / secs_per_year;

    // Cap at 100.0 — anything above this is displayed as ">100y"
    Some(years.min(100.0))
}

/// Format the years-left value for display in the output line.
/// - None            → "n/a"   (not enough data yet — less than 24 hours of history)
/// - Some(100.0)     → ">100y" (capped — effectively infinite at current rate)
/// - Some(x)         → "4.5y"  (one decimal place)
fn format_years_left(years: Option<f64>) -> String {
    match years {
        None => "n/a".to_string(),
        Some(y) if y >= 100.0 => ">100y".to_string(),
        Some(y) => format!("{:.1}y", y),
    }
}

// ─── Persistence ───

fn load_state(path: &PathBuf) -> State {
    fs::read_to_string(path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_else(State::new)
}

fn save_state(path: &PathBuf, state: &State) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let json = serde_json::to_string_pretty(state)?;
    fs::write(path, json)?;
    Ok(())
}

fn now_string() -> String {
    // Simple unix timestamp without pulling in chrono
    format!("{}", now_secs())
}

// ─── Main loop ───

fn main() -> Result<()> {
    let args = Args::parse();

    // ── Validate --initial-health if provided ──
    // Must be in the range 0.0 to 100.0 inclusive. We check this early so the
    // user gets a clear error message before any detection or state loading occurs.
    if let Some(ih) = args.initial_health {
        if !(0.0..=100.0).contains(&ih) {
            eprintln!(
                "Error: --initial-health value {:.2} is out of range. Must be between 0.0 and 100.0.",
                ih
            );
            std::process::exit(1);
        }
    }

    // ── Validate --over-provision ──
    // Must be in the range 0.0 to 0.5 — values above 50% are not physically
    // plausible for any real SD card and likely indicate a user error.
    if !(0.0..=0.5).contains(&args.over_provision) {
        eprintln!(
            "Error: --over-provision value {:.3} is out of range. Must be between 0.0 and 0.5.",
            args.over_provision
        );
        std::process::exit(1);
    }

    // ── Auto-detect card size from /sys/block/<dev>/size ──
    let card_bytes = detect_card_bytes(&args.device)
        .with_context(|| format!("Failed to detect size of device {}", args.device))?;
    let card_gb = card_bytes as f64 / (1024.0 * 1024.0 * 1024.0);

    // ── Erase block size: use override if provided, otherwise auto-detect ──
    // Note: erase block size is used for the lifetime budget display and P/E
    // accounting but NOT for WAF estimation (which now uses flash_page_size).
    let erase_block_bytes = match args.erase_block_kb {
        Some(kb) => {
            println!("Erase block:  {} KB (user override)", kb);
            kb * 1024
        }
        None => {
            let detected = detect_erase_block_bytes(&args.device);
            println!("Erase block:  {} KB (auto-detected)", detected / 1024);
            detected
        }
    };
    let erase_block_kb = erase_block_bytes / 1024;

    // ── Find all mount points for this device's partitions ──
    // This includes direct mounts and mounts via LUKS/dm-crypt device mapper volumes.
    let mount_points = find_mount_points(&args.device);

    // Build a flat list of mount point strings for storage in state JSON
    let mount_point_strings: Vec<String> = mount_points
        .iter()
        .map(|(dev, mp)| format!("{} -> {}", dev, mp))
        .collect();

    // The save interval as a Duration for time-based comparison
    let save_interval = Duration::from_secs(args.save_interval);

    // ── Determine whether a state file already exists before loading ──
    // We need to know this so we can decide whether to apply --initial-health.
    // load_state() silently falls back to State::new() if the file is absent or
    // unparseable, so we check existence explicitly here.
    let state_file_existed = args.state_file.exists();

    let mut state = load_state(&args.state_file);

    // ── Apply --initial-health only when starting fresh (no prior state file) ──
    //
    // If the user supplied --initial-health and there was no existing state file,
    // we initialise the life remaining and avg P/E cycle fields to reflect the
    // requested starting health rather than the default 100%.
    //
    // The avg P/E cycles value is back-calculated from the requested life %:
    //   life_remaining = (1 - avg_pe / pe_rated) * 100
    //   => avg_pe = (1 - life_remaining/100) * pe_rated
    //
    // If a state file already existed we leave everything untouched — the
    // persisted wear data is the ground truth and must not be overwritten.
    if let Some(initial_health) = args.initial_health {
        if !state_file_existed {
            // Back-calculate the implied avg P/E cycles from the requested health %
            let implied_avg_pe = (1.0 - initial_health / 100.0) * args.pe_cycles as f64;

            state.estimated_life_remaining_pct = initial_health;
            state.estimated_avg_pe_cycles = implied_avg_pe;
            state.initial_health_pct = initial_health;

            // Back-calculate implied estimated_flash_bytes_written so that subsequent
            // wear accumulation is consistent with the starting point.
            // formula: avg_pe = flash_bytes_written / card_bytes
            //          => flash_bytes_written = avg_pe * card_bytes
            state.estimated_flash_bytes_written =
                (implied_avg_pe * card_bytes as f64) as u64;

            println!(
                "Initial health preset to {:.1}% (--initial-health applied to new state file)",
                initial_health
            );
        } else {
            // State file already exists — silently ignore --initial-health
            println!(
                "Note: --initial-health ignored because an existing state file was found at {}",
                args.state_file.display()
            );
        }
    }

    // ── Refresh descriptor fields on every startup ──
    // These are always re-detected from the system so the JSON stays accurate
    // even if the card is swapped or the daemon is restarted with different args.
    state.device = args.device.clone();
    state.card_size_gb = (card_gb * 10.0).round() / 10.0; // round to 1 decimal place
    state.card_size_bytes = card_bytes;
    state.erase_block_kb = erase_block_kb;
    state.pe_cycles_rated = args.pe_cycles;
    state.mount_points = mount_point_strings.clone();

    // Read current kernel counters
    let current = read_kernel_stats(&args.device)?;

    // Detect reboot: kernel counters reset to 0 (or less than our last snapshot)
    if current.write_sectors < state.last_kernel_write_sectors {
        eprintln!(
            "Reboot detected (kernel sectors {} < stored {}). Continuing cumulative tracking.",
            current.write_sectors, state.last_kernel_write_sectors
        );
        state.reboot_count += 1;
    }

    // Set baseline to current kernel counters
    state.last_kernel_write_sectors = current.write_sectors;
    state.last_kernel_write_ios = current.write_ios;

    println!("sdwear — SD Card Flash Wear Estimator");
    println!("Device:        /dev/{}", args.device);
    println!("Card size:     {:.1} GB (auto-detected)", card_gb);
    println!("Erase block:   {} KB", erase_block_kb);
    println!("Flash page:    {} bytes", args.flash_page_size);
    println!("Over-prov:     {:.0}%", args.over_provision * 100.0);
    println!("Rated P/E:     {} cycles", args.pe_cycles);
    println!("State file:    {}", args.state_file.display());
    println!("Save interval: {} seconds", args.save_interval);

    // Report detected mount points
    if mount_points.is_empty() {
        println!("Mount points:  none detected");
    } else {
        for (dev, mp) in &mount_points {
            println!("Mount point:   {} → {}", dev, mp);
        }
    }

    println!(
        "Lifetime budget: {:.1} TB total flash writes",
        (card_bytes as f64 * args.pe_cycles as f64) / 1e12
    );
    println!("─────────────────────────────────────────────────────────────");
    println!(
        "Restored state: {:.6} avg P/E cycles, {:.4}% life remaining",
        state.estimated_avg_pe_cycles, state.estimated_life_remaining_pct
    );
    println!("─────────────────────────────────────────────────────────────");

    let mut prev = current;

    // Track wall-clock time since last save rather than counting ticks.
    // This means the save period is accurate regardless of poll interval setting.
    let mut last_save = Instant::now();

    loop {
        thread::sleep(Duration::from_secs(args.interval));

        let now = read_kernel_stats(&args.device)?;

        let delta_ios     = now.write_ios.saturating_sub(prev.write_ios);
        let delta_merges  = now.write_merges.saturating_sub(prev.write_merges);
        let delta_sectors = now.write_sectors.saturating_sub(prev.write_sectors);

        if delta_ios > 0 || delta_sectors > 0 {
            let delta_host_bytes = delta_sectors * 512;
            let delta_kb = delta_host_bytes / 1024;
            let avg_io_kb = if delta_ios > 0 {
                delta_kb as f64 / delta_ios as f64
            } else {
                0.0
            };

            // Sample current filesystem fullness.
            // We re-sample every poll so the estimate tracks changing disk usage.
            let fullness = max_filesystem_fullness(&mount_points);

            // Update the fullness in state so it is current at next save
            state.filesystem_fullness_pct = (fullness * 1000.0).round() / 10.0;

            // Estimate WAF using the new page-size + greedy-GC model.
            // Pass delta_merges so the model can derive the sequential ratio dynamically.
            let waf = estimate_waf(
                delta_sectors,
                delta_ios,
                delta_merges,
                args.flash_page_size,
                fullness,
                args.over_provision,
            );

            // ── Compute sequential ratio for display ──
            // Mirrors the calculation inside estimate_waf() so we can log it.
            let total_io_events = delta_ios + delta_merges;
            let seq_ratio = if total_io_events > 0 {
                delta_merges as f64 / total_io_events as f64
            } else {
                0.0
            };


            let delta_flash_bytes = (delta_host_bytes as f64 * waf) as u64;

            // Update cumulative state
            state.total_host_sectors_written += delta_sectors;
            state.total_host_write_ios += delta_ios;
            state.estimated_flash_bytes_written += delta_flash_bytes;
            state.last_kernel_write_sectors = now.write_sectors;
            state.last_kernel_write_ios = now.write_ios;
            state.last_updated = now_string();

            // ★ THE KEY NUMBER ★
            state.estimated_avg_pe_cycles =
                state.estimated_flash_bytes_written as f64 / card_bytes as f64;

            state.estimated_life_remaining_pct =
                ((1.0 - (state.estimated_avg_pe_cycles / args.pe_cycles as f64)) * 100.0)
                    .clamp(0.0, 100.0);

            // ── Push 24-hourly wear sample into the rolling ring buffer ──
            // Called on every write-active poll tick but internally only records
            // a sample once every 24 hours (WEAR_SAMPLE_INTERVAL_SECS).
            maybe_push_wear_sample(&mut state);

            // ── Compute years-left extrapolation from rolling wear rate ──
            let years_left = estimate_years_left(&state, card_bytes, args.pe_cycles);
            let years_str = format_years_left(years_left);

            // ── Print labeled key=value output line ──
            // Using key=value format so each line is self-describing in the
            // systemd journal even when the header has scrolled out of view.
            println!(
                "ios={} kb={} avg_kb={:.1} waf={:.2} seq={:.2} full={:.1}% pe={:.6} life={:.4}% yrs_left={}",
                delta_ios,
                delta_kb,
                avg_io_kb,
                waf,
                seq_ratio,
                fullness * 100.0,
                state.estimated_avg_pe_cycles,
                state.estimated_life_remaining_pct,
                years_str,
            );

        }

        prev = now;

        // Save state to disk when the configured save interval has elapsed.
        // Using wall-clock time (Instant) rather than tick counting means the
        // save period is accurate regardless of what --interval is set to.
        if last_save.elapsed() >= save_interval {
            save_state(&args.state_file, &state)?;
            last_save = Instant::now();
        }
    }
}
