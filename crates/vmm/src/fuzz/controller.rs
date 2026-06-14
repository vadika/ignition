//! Host-side fuzzer brain for M0: snapshot/reset bookkeeping, blind mutation,
//! and crash capture. The vCPU register save/restore lives behind `capture`/
//! `reset` (HVF thread-affine, called on the vCPU thread); the memory reset,
//! mutator, and solution writer are pure and tested here.

use std::path::{Path, PathBuf};

use ignition_hvf::{HvfVcpu, VcpuState};

/// Deterministic xorshift64* PRNG. A fixed seed makes a fuzz run reproducible,
/// which the determinism requirements (spec §7) depend on.
pub struct Rng {
    state: u64,
}

impl Rng {
    pub fn new(seed: u64) -> Rng {
        // Avoid the all-zero fixed point.
        Rng { state: if seed == 0 { 0x9E37_79B9_7F4A_7C15 } else { seed } }
    }
    pub fn next_u64(&mut self) -> u64 {
        let mut x = self.state;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.state = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    pub fn below(&mut self, n: usize) -> usize {
        if n == 0 { 0 } else { (self.next_u64() % n as u64) as usize }
    }
}

/// Blind "havoc-lite" mutation in place: a handful of random byte sets / bit
/// flips on a copy of `seed`, clamped to `max_len`. No coverage feedback (that
/// is M2). Returns the mutated bytes.
pub fn mutate(seed: &[u8], rng: &mut Rng, max_len: usize) -> Vec<u8> {
    let mut out = seed.to_vec();
    if out.is_empty() {
        out.push(0);
    }
    if out.len() > max_len {
        out.truncate(max_len.max(1));
    }
    let rounds = 1 + rng.below(8);
    for _ in 0..rounds {
        let i = rng.below(out.len());
        match rng.below(3) {
            0 => out[i] = rng.next_u64() as u8,            // random byte
            1 => out[i] ^= 1u8 << rng.below(8),            // bit flip
            _ => out[i] = out[i].wrapping_add(1),          // increment
        }
    }
    out
}

/// Reset guest RAM to the captured base by overwriting every byte. v0 of the
/// spec's §6 reset: correct and simple, no dirty tracking. `base` and `live`
/// must be the same length (full guest RAM).
pub fn restore_ram(base: &[u8], live: &mut [u8]) {
    debug_assert_eq!(base.len(), live.len(), "base and live RAM must match in size");
    live.copy_from_slice(base);
}

/// Copy a fixed input verbatim into the window (replay/determinism mode). No
/// mutation — used to confirm a saved crash input reproduces deterministically.
pub fn replay_into(input: &[u8], window: &mut [u8]) -> u32 {
    let n = input.len().min(window.len());
    window[..n].copy_from_slice(&input[..n]);
    n as u32
}

/// Write a crash-triggering input and its metadata to the solutions directory.
/// Returns the path of the written input file.
pub fn write_solution(dir: &Path, index: u64, input: &[u8], crash_code: u32) -> std::io::Result<PathBuf> {
    std::fs::create_dir_all(dir)?;
    let input_path = dir.join(format!("crash-{index:06}.bin"));
    std::fs::write(&input_path, input)?;
    std::fs::write(
        dir.join(format!("crash-{index:06}.meta")),
        format!("crash_code={crash_code}\nlen={}\n", input.len()),
    )?;
    Ok(input_path)
}

/// Accumulated edge-coverage map (the host-side "virgin bits"). Each `record`
/// folds one iteration's freshly-read 8-bit counter buffer in: an index that is
/// nonzero now but was never seen before is new coverage. Counts, not just bits,
/// are read from the guest, but only first-touch is tracked — enough for the M2
/// coverage curve and the coverage-guided corpus. (libAFL's bucketed
/// `MaxMapFeedback` is the later, richer replacement.)
pub struct CoverageMap {
    seen: Vec<bool>,
    covered: usize,
}

impl CoverageMap {
    pub fn new(len: usize) -> CoverageMap {
        CoverageMap { seen: vec![false; len], covered: 0 }
    }

    /// Fold `cov` (this iteration's counters) into the accumulated map. Returns
    /// true if any previously-unseen edge was hit. `cov` may be shorter or longer
    /// than the map; only the overlapping prefix is considered.
    pub fn record(&mut self, cov: &[u8]) -> bool {
        let mut new = false;
        let n = cov.len().min(self.seen.len());
        for i in 0..n {
            if cov[i] != 0 && !self.seen[i] {
                self.seen[i] = true;
                self.covered += 1;
                new = true;
            }
        }
        new
    }

    /// Total distinct edges hit across all recorded iterations.
    pub fn covered(&self) -> usize {
        self.covered
    }
}

/// The live fuzzer state for one M0 run. Holds the host-side base copy of guest
/// RAM, the saved base register file, a raw view of live guest RAM and the
/// shared window (host VAs from the boot harness's mmaps), the mutator, the seed
/// corpus, and the solutions directory.
///
/// SAFETY: `ram_ptr`/`window_ptr` are host pointers to mappings that outlive the
/// fuzz run (owned by the boot harness). The controller is used only on the
/// single vCPU thread, so the &mut slices it forms are never aliased.
pub struct FuzzController {
    base_ram: Vec<u8>,
    base_state: Option<VcpuState>,
    ram_ptr: *mut u8,
    ram_len: usize,
    window_ptr: *mut u8,
    window_len: usize,
    rng: Rng,
    seeds: Vec<Vec<u8>>,
    seed_idx: usize,
    replay: Option<Vec<u8>>,
    solutions_dir: PathBuf,
    crash_count: u64,
    iterations: u64,
    captured: bool,
}

// The controller lives on one thread; the raw pointers are not shared.
unsafe impl Send for FuzzController {}

impl FuzzController {
    /// `ram`/`window` are (ptr, len) of the host mappings for guest RAM and the
    /// shared window. `seeds` is the starting corpus (may be empty). `replay`, if
    /// `Some`, is a fixed input fed verbatim every iteration (no mutation) for the
    /// determinism gate; it takes precedence over the seed corpus. `seed_rng`
    /// fixes the mutation stream for reproducibility.
    pub fn new(
        ram: (*mut u8, usize),
        window: (*mut u8, usize),
        seeds: Vec<Vec<u8>>,
        replay: Option<Vec<u8>>,
        seed_rng: u64,
        solutions_dir: PathBuf,
    ) -> FuzzController {
        FuzzController {
            base_ram: Vec::new(),
            base_state: None,
            ram_ptr: ram.0,
            ram_len: ram.1,
            window_ptr: window.0,
            window_len: window.1,
            rng: Rng::new(seed_rng),
            seeds: if seeds.is_empty() { vec![vec![0u8; 1]] } else { seeds },
            seed_idx: 0,
            replay,
            solutions_dir,
            crash_count: 0,
            iterations: 0,
            captured: false,
        }
    }

    pub fn is_captured(&self) -> bool { self.captured }
    pub fn iterations(&self) -> u64 { self.iterations }
    pub fn crash_count(&self) -> u64 { self.crash_count }

    fn live_ram(&mut self) -> &mut [u8] {
        // SAFETY: see struct doc; single-threaded, mapping outlives the run.
        unsafe { std::slice::from_raw_parts_mut(self.ram_ptr, self.ram_len) }
    }
    fn window(&mut self) -> &mut [u8] {
        // SAFETY: see struct doc; single-threaded, mapping outlives the run.
        unsafe { std::slice::from_raw_parts_mut(self.window_ptr, self.window_len) }
    }

    /// One-time SNAPSHOT_ME handling: PC is advanced past the doorbell store by
    /// the caller; copy guest RAM into the base buffer and save the register
    /// file. Returns the first input length to expose to the guest.
    pub fn capture(&mut self, vcpu: &HvfVcpu) -> Result<u32, ignition_hvf::Error> {
        let live = self.live_ram().to_vec();
        self.base_ram = live;
        self.base_state = Some(vcpu.save_state()?);
        self.captured = true;
        Ok(self.prepare_next_input())
    }

    /// Pick the next seed, mutate it into the shared window, return its length.
    fn prepare_next_input(&mut self) -> u32 {
        if let Some(fixed) = self.replay.clone() {
            return replay_into(&fixed, self.window());
        }
        let seed = self.seeds[self.seed_idx % self.seeds.len()].clone();
        self.seed_idx = self.seed_idx.wrapping_add(1);
        let max = self.window_len;
        let input = mutate(&seed, &mut self.rng, max);
        let n = input.len().min(self.window_len);
        self.window()[..n].copy_from_slice(&input[..n]);
        n as u32
    }

    /// DONE handling: count the iteration, prepare the next input, reset.
    pub fn on_done(&mut self, vcpu: &mut HvfVcpu) -> Result<u32, ignition_hvf::Error> {
        self.iterations += 1;
        let len = self.prepare_next_input();
        self.reset(vcpu)?;
        Ok(len)
    }

    /// CRASH handling: record the current input as a solution, then behave like
    /// DONE. `crash_code` and `input_len` come from the device.
    pub fn on_crash(&mut self, vcpu: &mut HvfVcpu, crash_code: u32, input_len: u32) -> Result<u32, ignition_hvf::Error> {
        let n = (input_len as usize).min(self.window_len);
        let input = self.window()[..n].to_vec();
        if let Err(e) = write_solution(&self.solutions_dir, self.crash_count, &input, crash_code) {
            log::warn!("failed to write fuzz solution: {e}");
        }
        self.crash_count += 1;
        log::info!("fuzz: CRASH captured (code={crash_code}, len={n}), solutions={}", self.crash_count);
        self.iterations += 1;
        let len = self.prepare_next_input();
        self.reset(vcpu)?;
        Ok(len)
    }

    /// Roll the guest back to the snapshot: memcpy base->live RAM, restore the
    /// register file, and cancel the pending lazy PC advance from the doorbell
    /// trap (restore_state already set PC to the post-SNAPSHOT_ME value).
    fn reset(&mut self, vcpu: &mut HvfVcpu) -> Result<(), ignition_hvf::Error> {
        let base = std::mem::take(&mut self.base_ram);
        restore_ram(&base, self.live_ram());
        self.base_ram = base;
        let state = self.base_state.as_ref().expect("reset before capture");
        vcpu.restore_state(state)?;
        vcpu.clear_pending_advance();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rng_is_deterministic_for_a_seed() {
        let mut a = Rng::new(42);
        let mut b = Rng::new(42);
        for _ in 0..100 {
            assert_eq!(a.next_u64(), b.next_u64());
        }
    }

    #[test]
    fn mutate_is_deterministic_and_bounded() {
        let seed = b"hello world".to_vec();
        let mut r1 = Rng::new(7);
        let mut r2 = Rng::new(7);
        let m1 = mutate(&seed, &mut r1, 64);
        let m2 = mutate(&seed, &mut r2, 64);
        assert_eq!(m1, m2, "same seed -> same mutation");
        assert!(m1.len() <= 64);
    }

    #[test]
    fn mutate_handles_empty_seed() {
        let mut r = Rng::new(1);
        let m = mutate(&[], &mut r, 64);
        assert!(!m.is_empty());
    }

    #[test]
    fn restore_ram_overwrites_dirtied_bytes() {
        let base = vec![0xAAu8; 4096];
        let mut live = base.clone();
        live[10] = 0x55;
        live[4000] = 0x11;
        restore_ram(&base, &mut live);
        assert_eq!(live, base);
    }

    #[test]
    fn replay_into_copies_verbatim_clamped() {
        let mut win = vec![0u8; 4];
        let n = replay_into(&[0xAB, 0xCD, 0xEF], &mut win);
        assert_eq!(n, 3);
        assert_eq!(&win[..3], &[0xAB, 0xCD, 0xEF]);
        // clamps to window length
        let mut small = vec![0u8; 2];
        assert_eq!(replay_into(&[1, 2, 3, 4], &mut small), 2);
        assert_eq!(&small[..], &[1, 2]);
    }

    #[test]
    fn write_solution_emits_input_and_meta() {
        let dir = std::env::temp_dir().join(format!("ignition-fuzz-sol-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let p = write_solution(&dir, 0, b"\xde\xad", 9).unwrap();
        assert_eq!(std::fs::read(&p).unwrap(), b"\xde\xad");
        let meta = std::fs::read_to_string(dir.join("crash-000000.meta")).unwrap();
        assert!(meta.contains("crash_code=9"));
        assert!(meta.contains("len=2"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn coverage_map_reports_new_edges_then_saturates() {
        let mut cm = CoverageMap::new(8);
        // First observation: edges 1 and 4 hit -> new coverage.
        assert!(cm.record(&[0, 5, 0, 0, 2, 0, 0, 0]));
        assert_eq!(cm.covered(), 2);
        // Same edges again (different counts) -> no new coverage.
        assert!(!cm.record(&[0, 1, 0, 0, 9, 0, 0, 0]));
        assert_eq!(cm.covered(), 2);
        // A new edge (index 7) -> new coverage.
        assert!(cm.record(&[0, 0, 0, 0, 0, 0, 0, 3]));
        assert_eq!(cm.covered(), 3);
    }

    #[test]
    fn coverage_map_all_zero_is_not_new() {
        let mut cm = CoverageMap::new(4);
        assert!(!cm.record(&[0, 0, 0, 0]));
        assert_eq!(cm.covered(), 0);
    }
}
