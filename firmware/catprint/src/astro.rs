//! Calendar and sky arithmetic, all from the Unix time: no network needed.
use core::f32::consts::PI;

/// Days since 1970-01-01 to (year, month, day), proleptic Gregorian.
pub fn civil(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (yoe + era * 400 + (m <= 2) as i64, m as u32, d as u32)
}

pub fn weekday(unix: i64) -> &'static str {
    ["THU", "FRI", "SAT", "SUN", "MON", "TUE", "WED"][unix.div_euclid(86_400).rem_euclid(7) as usize]
}

/// (year, month, day) to days since 1970-01-01.
pub fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let mp = (m as i64 + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// 1 on January 1st.
pub fn day_of_year(unix: i64) -> u32 {
    let days = unix.div_euclid(86_400);
    (days - days_from_civil(civil(days).0, 1, 1) + 1) as u32
}

const SYNODIC: f32 = 29.530_588;
/// A new moon: 2000-01-06 18:14 UTC.
const NEW_MOON_UNIX: i64 = 947_182_440;

/// Moon age in days since the last new moon.
pub fn moon_age(unix: i64) -> f32 {
    let days = (unix - NEW_MOON_UNIX) as f64 / 86_400.0;
    (days % SYNODIC as f64) as f32
}

/// Fraction of the disc that is lit.
pub fn moon_lit(age: f32) -> f32 {
    (1.0 - libm::cosf(2.0 * PI * age / SYNODIC)) / 2.0
}

pub fn moon_name(age: f32) -> &'static str {
    let names = ["NEW MOON", "WAXING CRESCENT", "FIRST QUARTER", "WAXING GIBBOUS", "FULL MOON", "WANING GIBBOUS", "LAST QUARTER", "WANING CRESCENT"];
    names[((age / SYNODIC * 8.0 + 0.5) as usize) % 8]
}

/// Phase angle, 0 at new moon, PI at full.
pub fn moon_angle(age: f32) -> f32 {
    2.0 * PI * age / SYNODIC
}

/// Where the sun is overhead: (latitude, longitude) in degrees. Good to a
/// degree or so (no equation of time).
pub fn subsolar(unix: i64) -> (f32, f32) {
    let doy = day_of_year(unix) as f32;
    let decl = -23.44 * libm::cosf(2.0 * PI / 365.0 * (doy + 10.0));
    let hours = unix.rem_euclid(86_400) as f32 / 3600.0;
    let mut lon = -15.0 * (hours - 12.0);
    if lon < -180.0 {
        lon += 360.0;
    }
    (decl, lon)
}

/// Great-circle distance in km.
pub fn distance_km(a: (f32, f32), b: (f32, f32)) -> f32 {
    let r = PI / 180.0;
    let (dlat, dlon) = ((b.0 - a.0) * r, (b.1 - a.1) * r);
    let h = libm::sinf(dlat / 2.0) * libm::sinf(dlat / 2.0) + libm::cosf(a.0 * r) * libm::cosf(b.0 * r) * libm::sinf(dlon / 2.0) * libm::sinf(dlon / 2.0);
    2.0 * 6371.0 * libm::asinf(libm::sqrtf(h).min(1.0))
}
