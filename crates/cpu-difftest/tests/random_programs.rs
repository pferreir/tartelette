//! Random instruction sequences run in lock-step on both sides, comparing
//! full state after every instruction. Catches state that leaks between
//! instructions (flags, SP banking, writeback) which single-step can't.

mod common;
use common::*;

const MAX_STEPS: usize = 48;
/// Halfwords of generated code placed at CODE.
const PROGRAM_LEN: usize = 160;

/// A random 16-bit instruction, biased towards ones that retire (data
/// processing, loads/stores, SP adjusts, short branches) so chains run long.
fn random_insn(rng: &mut fastrand::Rng) -> u16 {
    loop {
        let hw = match rng.u8(..10) {
            0..=7 => rng.u16(0..0xB000),  // everything up to the misc group
            8 => 0xB000 | rng.u16(..0x1000), // misc: SP adjust, extend, push/pop, rev, hints
            _ => 0xD000 | rng.u16(..0x1000), // conditional branches, B
        };
        if !unpredictable(hw) && !v7m_only(hw, None) && !is_wide(hw) {
            return hw;
        }
    }
}

#[test]
fn random_programs() {
    let seed = seed() ^ 0xC0DE;
    let mut rng = fastrand::Rng::with_seed(seed);
    let mut pair = Pair::new();
    let programs = 4000 * rounds() as usize;

    let mut steps_ok = 0usize;
    let mut ended: std::collections::BTreeMap<String, usize> = Default::default();
    let mut deviations = 0usize;
    let mut failures = vec![];

    'program: for p in 0..programs {
        let code: Vec<u16> = (0..PROGRAM_LEN).map(|_| random_insn(&mut rng)).collect();
        let mut st = State::random(&mut rng);
        // nPRIV is not modelled (see not_comparable); keep CONTROL writes sane
        st.pc = CODE;
        let mem = random_mem(&mut rng, &code);
        pair.load(&st, &mem);

        let mut trace = vec![];
        for step in 0..MAX_STEPS {
            let before = State::of_cpu(&pair.cpu);
            let off = before.pc.wrapping_sub(BASE) as usize;
            if before.pc < BASE || off + 4 > SIZE {
                *ended.entry("pc left RAM".into()).or_default() += 1;
                continue 'program;
            }
            let hw = u16::from_le_bytes([pair.bus.mem[off], pair.bus.mem[off + 1]]);
            let hw2 = is_wide(hw).then(|| u16::from_le_bytes([pair.bus.mem[off + 2], pair.bus.mem[off + 3]]));
            if unpredictable(hw) || not_comparable(hw, hw2, &before) {
                *ended.entry("unpredictable insn".into()).or_default() += 1;
                continue 'program;
            }
            trace.push(format!("{:08x}: {}", before.pc, describe(hw, hw2)));

            let res = pair.step();
            match judge(&res, &pair, hw, hw2, &before) {
                Verdict::Match => {}
                Verdict::Deviation(_) => {
                    deviations += 1;
                    continue 'program;
                }
                Verdict::Mismatch(why) => {
                    if failures.len() < 5 {
                        failures.push(format!(
                            "program #{p} (seed {seed:#x}) step {step}: {why}\n  initial={st:x?}\n  trace:\n    {}",
                            trace.join("\n    ")
                        ));
                    }
                    continue 'program;
                }
            }
            if pair.cpu.sleeping {
                // WFI/WFE: our core idles from here, Unicorn keeps executing
                *ended.entry("sleep".into()).or_default() += 1;
                continue 'program;
            }
            if res.ours != Class::Ok || res.uc_err.is_some() {
                // both trapped / faulted equivalently: the chain ends here
                *ended.entry(format!("{:?}", res.ours)).or_default() += 1;
                continue 'program;
            }
            steps_ok += 1;
        }
        *ended.entry("max steps".into()).or_default() += 1;
    }

    eprintln!("{programs} programs, {steps_ok} lock-step instructions compared, {deviations} ended on a known deviation");
    eprintln!("chains ended by: {ended:?}");
    for f in &failures {
        eprintln!("{f}");
    }
    assert!(failures.is_empty(), "{} diverging programs (first {} shown)", failures.len(), failures.len());
}
