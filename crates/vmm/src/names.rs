//! Memorable snapshot-name generator: `adjective-surname` (e.g. `brave-hopper`).
//! No external RNG — the seed mixes the wall clock with the pid via one splitmix64
//! step, so successive calls in a process differ as the clock advances.

use std::time::{SystemTime, UNIX_EPOCH};

const ADJECTIVES: &[&str] = &[
    "amber", "bold", "brave", "bright", "calm", "clever", "cosmic", "crimson",
    "curious", "daring", "eager", "fancy", "gentle", "golden", "happy", "hidden",
    "jolly", "keen", "lively", "lucid", "mellow", "nimble", "noble", "proud",
    "quiet", "rapid", "shiny", "silent", "smooth", "solar", "spry", "stellar",
    "swift", "tidy", "vivid", "witty", "zesty", "azure", "lunar", "mighty",
];

const SURNAMES: &[&str] = &[
    "archimedes", "babbage", "bohr", "curie", "darwin", "dirac", "einstein", "euler",
    "faraday", "fermi", "feynman", "franklin", "galileo", "galois", "gauss", "goodall",
    "hawking", "heisenberg", "hopper", "hubble", "kepler", "lamarr", "lovelace",
    "maxwell", "mendel", "newton", "noether", "pasteur", "pauli", "planck",
    "ramanujan", "sagan", "shannon", "tesla", "thompson", "turing", "volta",
    "watt", "wozniak", "yonath",
];

/// Mix the wall clock and pid into a 64-bit value via one splitmix64 round.
fn seed() -> u64 {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    let mut z = nanos ^ (std::process::id() as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// A random `adjective-surname` name.
pub fn generate() -> String {
    let s = seed();
    let adj = ADJECTIVES[(s % ADJECTIVES.len() as u64) as usize];
    let sur = SURNAMES[((s >> 32) % SURNAMES.len() as u64) as usize];
    format!("{adj}-{sur}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generate_is_well_formed() {
        let n = generate();
        let (a, b) = n.split_once('-').expect("name should contain a hyphen");
        assert!(ADJECTIVES.contains(&a), "adjective part {a:?} not in list");
        assert!(SURNAMES.contains(&b), "surname part {b:?} not in list");
    }
}
