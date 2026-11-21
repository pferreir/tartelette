//! Every 16-bit encoding (plus targeted 32-bit ones) from random states,
//! one instruction at a time, against Unicorn.

mod common;
use std::collections::BTreeMap;

use common::*;

/// Second halfword for a 32-bit encoding starting with `hw1`. Prefixes in
/// 0xF000..=0xF7FF are shared by BL, MSR, MRS and the barriers, which differ
/// only in the second halfword, so pick the form that fits `hw1` (or BL).
fn wide_hw2(rng: &mut fastrand::Rng, hw1: u16) -> u16 {
    let bl = 0xD000 | rng.u16(..) & 0x2FFF;
    match (hw1, rng.u8(..4)) {
        (0xF380..=0xF38F, 0..=2) => 0x8800 | rng.u16(0..32), // MSR
        (0xF3EF, 0..=2) => 0x8000 | rng.u16(0..16) << 8 | rng.u16(0..32), // MRS
        (0xF3BF, 0..=2) => 0x8F00 | rng.u16(..) & 0xFF,      // barriers
        (0xF000..=0xF7FF, 0..=2) => bl,
        _ => rng.u16(..),
    }
}

#[test]
fn single_instruction_sweep() {
    let mut rng = fastrand::Rng::with_seed(seed());
    let mut pair = Pair::new();
    let mut stats: BTreeMap<String, u32> = BTreeMap::new();
    let mut failures: BTreeMap<String, (u32, Vec<String>, String)> = BTreeMap::new();

    let mut encodings: Vec<u16> = (0..=0xFFFF).collect();
    // extra wide coverage: the special-register / barrier prefixes
    for hw1 in [0xF380u16, 0xF383, 0xF38F, 0xF3EF, 0xF3BF] {
        encodings.extend(std::iter::repeat_n(hw1, 64));
    }

    // DIFFTEST_ONLY=f380 restricts the sweep to one encoding and prints every failure.
    let only = std::env::var("DIFFTEST_ONLY")
        .ok()
        .and_then(|s| u16::from_str_radix(&s, 16).ok());
    if let Some(o) = only {
        encodings.retain(|&h| h == o);
        encodings.extend(std::iter::repeat_n(o, 200));
    }
    let mut deviations: BTreeMap<&str, u32> = BTreeMap::new();
    for hw in encodings.into_iter().filter(|&h| !unpredictable(h)) {
        for _ in 0..rounds() {
            let hw2 = is_wide(hw).then(|| wide_hw2(&mut rng, hw));
            let st = State::random(&mut rng);
            if not_comparable(hw, hw2, &st) {
                continue;
            }
            let mem = random_mem(&mut rng, &[hw, hw2.unwrap_or(0)]);
            pair.load(&st, &mem);
            let res = pair.step();
            *stats
                .entry(format!("{:?}/{:?}", res.ours, res.theirs))
                .or_default() += 1;

            match judge(&res, &pair, hw, hw2, &st) {
                Verdict::Match => {}
                Verdict::Deviation(why) => *deviations.entry(why).or_default() += 1,
                Verdict::Mismatch(p) => {
                    let cat = format!("{:02x}xx", hw >> 8);
                    let v = failures.entry(cat).or_default();
                    v.0 += 1;
                    if only.is_some() {
                        eprintln!("{} :: {}\n    state={:x?}", describe(hw, hw2), p, st);
                    }
                    if v.1.len() < 8 {
                        v.1.push(describe(hw, hw2));
                    }
                    if v.2.is_empty() {
                        v.2 = format!("{} :: {}\n    state={:x?}", describe(hw, hw2), p, st);
                    }
                }
            }
        }
    }
    eprintln!("known QEMU deviations skipped: {deviations:?}");
    eprintln!("outcome (ours/unicorn): {stats:?}");
    for (k, (n, hws, example)) in &failures {
        eprintln!("[{k}] x{n}: {}\n  e.g. {example}", hws.join(" "));
    }
    assert!(
        failures.is_empty(),
        "{} mismatching categories",
        failures.len()
    );
}
