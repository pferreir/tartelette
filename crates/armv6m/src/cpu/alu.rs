//! Flag-producing arithmetic and shift helpers.

pub(super) fn add_with_carry(a: u32, b: u32, carry: bool) -> (u32, bool, bool) {
    let wide = a as u64 + b as u64 + carry as u64;
    let res = wide as u32;
    let c = wide > u32::MAX as u64;
    let v = ((a ^ res) & (b ^ res)) >> 31 != 0;
    (res, c, v)
}

// Shift helpers: `n` is the shift amount (0..=255), `c` the incoming carry.
pub(super) fn lsl(val: u32, n: u32, c: bool) -> (u32, bool) {
    match n {
        0 => (val, c),
        1..=31 => (val << n, (val >> (32 - n)) & 1 != 0),
        32 => (0, val & 1 != 0),
        _ => (0, false),
    }
}
pub(super) fn lsr(val: u32, n: u32, c: bool) -> (u32, bool) {
    match n {
        0 => (val, c),
        1..=31 => (val >> n, (val >> (n - 1)) & 1 != 0),
        32 => (0, val >> 31 != 0),
        _ => (0, false),
    }
}
pub(super) fn asr(val: u32, n: u32, c: bool) -> (u32, bool) {
    match n {
        0 => (val, c),
        1..=31 => (((val as i32) >> n) as u32, (val >> (n - 1)) & 1 != 0),
        _ => (((val as i32) >> 31) as u32, val >> 31 != 0),
    }
}
pub(super) fn ror(val: u32, n: u32, c: bool) -> (u32, bool) {
    if n == 0 {
        (val, c)
    } else {
        let res = val.rotate_right(n & 31);
        (res, res >> 31 != 0)
    }
}
