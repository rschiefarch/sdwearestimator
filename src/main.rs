// src/main.rs
// altered by Poindexter AI: added --save-interval argument to replace hardcoded 60-tick save period
// altered by Poindexter AI: auto-detect card size, erase block size, and filesystem fullness for WAF multiplier
// altered by Poindexter AI: added human-readable descriptor fields to persisted state JSON
// altered by Poindexter AI: fix erase block zero bug; add LUKS/dm-crypt mount detection via /sys/block/<dev>/holders
// altered by Poindexter AI: add #[serde(default)] to all State fields for backwards compatibility with older state files
// altered by Poindexter AI: added --initial-health argument to preset starting life % for a card already in use

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

// ─── Persistent state: survives reboots ───
//
// This struct is serialised to JSON. It is designed to be human-readable
// so that an operator can open the file and immediately understand the
// health of the SD card without needing to know the program arguments
// that were used to start the daemon.
//
// Fields are grouped into three sections:
//   1. Descriptor fields  — what device/card this file relates to
//   2. Cumulative metrics — the wear tracking numbers
//   3. Internal counters  — used by the algorithm for delta/reboot detection
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
        }
    }
}

// ─── Reading /sys/block/<dev>/stat ───

struct KernelStats {
    write_ios: u64,
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
        write_ios: *fields.get(4).unwrap_or(&0),
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

// ─── Fullness WAF multiplier ───
//
// As the card fills up, the FTL has less free space to work with.
// Garbage collection is forced to run more frequently and with less
// choice about which blocks to consolidate, increasing WAF.
//
// These multipliers are heuristic — real values depend on the FTL
// implementation which is proprietary. The curve is intentionally
// conservative (not alarmist).
//
//   < 50% full  → no extra pressure, multiplier = 1.0
//   50-70%      → mild pressure,     multiplier = 1.1
//   70-85%      → moderate pressure, multiplier = 1.3
//   85-95%      → high pressure,     multiplier = 1.6
//   > 95%       → severe pressure,   multiplier = 2.0

fn fullness_waf_multiplier(used_fraction: f64) -> f64 {
    match used_fraction {
        f if f >= 0.95 => 2.0,
        f if f >= 0.85 => 1.6,
        f if f >= 0.70 => 1.3,
        f if f >= 0.50 => 1.1,
        _ => 1.0,
    }
}

// ─── Write amplification estimation ───
//
// The only thing we can observe from outside the SD card is:
//   - How many IOs the host sent
//   - How many sectors total
//   - Therefore average IO size
//
// Small random writes cause the FTL to do read-modify-erase-write
// on entire erase blocks. Large sequential writes map ~1:1.
//
// We model base WAF as a function of (avg_io_size / erase_block_size),
// then apply a fullness multiplier to account for GC pressure.

fn estimate_waf(
    delta_sectors: u64,
    delta_ios: u64,
    erase_block_bytes: u64,
    fullness_multiplier: f64,
) -> f64 {
    if delta_ios == 0 || delta_sectors == 0 {
        return 1.0;
    }

    // Guard against a zero erase block size — should not happen after the
    // detection fix but we defend here as well to avoid a divide-by-zero panic
    if erase_block_bytes == 0 {
        return 1.0;
    }

    let avg_io_bytes = (delta_sectors * 512) as f64 / delta_ios as f64;

    // Base WAF from IO size vs erase block size
    let base_waf = if avg_io_bytes >= erase_block_bytes as f64 {
        // Writes are already erase-block sized or larger — nearly 1:1
        1.05
    } else {
        // Naive worst case: full erase block rewritten per small write
        let naive_waf = erase_block_bytes as f64 / avg_io_bytes;

        // Real FTLs use log-structured translation, so actual WAF is
        // much less than naive. Factor of ~0.1-0.2 is reasonable for
        // consumer SD cards with decent controllers.
        let ftl_efficiency = 0.15;
        let waf = 1.0 + (naive_waf - 1.0) * ftl_efficiency;
        waf.clamp(1.0, naive_waf)
    };

    // Apply fullness multiplier — higher card usage = more GC pressure = higher WAF
    base_waf * fullness_multiplier
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
    let d = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    format!("{}", d.as_secs())
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

    // ── Auto-detect card size from /sys/block/<dev>/size ──
    let card_bytes = detect_card_bytes(&args.device)
        .with_context(|| format!("Failed to detect size of device {}", args.device))?;
    let card_gb = card_bytes as f64 / (1024.0 * 1024.0 * 1024.0);

    // ── Erase block size: use override if provided, otherwise auto-detect ──
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
    println!(
        "{:>10} {:>10} {:>8} {:>6} {:>8} {:>12} {:>8}",
        "Δ IOs", "Δ KB", "Avg KB", "WAF", "Full%", "Avg P/E", "Life %"
    );

    let mut prev = current;

    // Track wall-clock time since last save rather than counting ticks.
    // This means the save period is accurate regardless of poll interval setting.
    let mut last_save = Instant::now();

    loop {
        thread::sleep(Duration::from_secs(args.interval));

        let now = read_kernel_stats(&args.device)?;

        let delta_ios = now.write_ios.saturating_sub(prev.write_ios);
        let delta_sectors = now.write_sectors.saturating_sub(prev.write_sectors);

        if delta_ios > 0 || delta_sectors > 0 {
            let delta_host_bytes = delta_sectors * 512;
            let delta_kb = delta_host_bytes / 1024;
            let avg_io_kb = if delta_ios > 0 {
                delta_kb as f64 / delta_ios as f64
            } else {
                0.0
            };

            // Sample current filesystem fullness and compute WAF multiplier.
            // We re-sample every poll so the estimate tracks changing disk usage.
            let fullness = max_filesystem_fullness(&mount_points);
            let fullness_mult = fullness_waf_multiplier(fullness);

            // Update the fullness in state so it is current at next save
            state.filesystem_fullness_pct = (fullness * 1000.0).round() / 10.0;

            let waf = estimate_waf(delta_sectors, delta_ios, erase_block_bytes, fullness_mult);
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

            println!(
                "{:>10} {:>10} {:>8.1} {:>6.2} {:>7.1}% {:>12.6} {:>7.4}%",
                delta_ios,
                delta_kb,
                avg_io_kb,
                waf,
                fullness * 100.0,
                state.estimated_avg_pe_cycles,
                state.estimated_life_remaining_pct,
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
