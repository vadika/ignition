//! Host-side fuzzer brain for M0: snapshot/reset bookkeeping, blind mutation,
//! and crash capture. The vCPU register save/restore lives behind `capture`/
//! `reset` (HVF thread-affine, called on the vCPU thread); the memory reset,
//! mutator, and solution writer are pure and tested here.

use std::path::{Path, PathBuf};

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
}
