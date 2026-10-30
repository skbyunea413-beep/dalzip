use crate::crypto::KdfParams;
use crate::format::{FLAG_ENCRYPTED, MAGIC};
use anyhow::{bail, Result};
use std::io::Read;

pub fn human(n: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut v = n as f64;
    let mut u = 0;
    while v >= 1024.0 && u < UNITS.len() - 1 {
        v /= 1024.0;
        u += 1;
    }
    if u == 0 {
        format!("{n} B")
    } else {
        format!("{v:.2} {}", UNITS[u])
    }
}

/// Calendar fields (year, month, day, hour, minute, second) of a Unix time.
pub fn civil(secs: i64) -> (i64, i64, i64, i64, i64, i64) {
    // Civil-from-days, enough without a date crate.
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + (m <= 2) as i64;
    (y, m, d, rem / 3600, rem % 3600 / 60, rem % 60)
}

/// Inverse of `civil`: Unix time of a calendar instant (days-from-civil).
pub fn unix_from_civil(y: i64, m: i64, d: i64, h: i64, mi: i64, s: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    days * 86_400 + h * 3600 + mi * 60 + s
}

/// `YYYY-MM-DD HH:MM` for a Unix time, in UTC plus `offset_secs`.
pub fn fmt_time_at(secs: i64, offset_secs: i64) -> String {
    let (y, m, d, h, mi, _) = civil(secs + offset_secs);
    format!("{y:04}-{m:02}-{d:02} {h:02}:{mi:02}")
}

/// Seconds to add to UTC for local time (standard time; no DST rules).
pub fn local_offset_secs() -> i64 {
    #[cfg(windows)]
    {
        use windows_sys::Win32::System::Time::{GetTimeZoneInformation, TIME_ZONE_INFORMATION};
        let mut tz: TIME_ZONE_INFORMATION = unsafe { std::mem::zeroed() };
        unsafe { GetTimeZoneInformation(&mut tz) };
        -(tz.Bias as i64) * 60
    }
    #[cfg(not(windows))]
    0
}

pub fn fmt_time(secs: i64) -> String {
    fmt_time_at(secs, 0)
}

pub fn write_header(kdf: Option<&KdfParams>) -> Vec<u8> {
    let mut h = MAGIC.to_vec();
    match kdf {
        None => h.push(0),
        Some(k) => {
            h.push(FLAG_ENCRYPTED);
            h.extend_from_slice(&k.salt);
            for v in [k.m_kib, k.t, k.p] {
                h.extend_from_slice(&v.to_le_bytes());
            }
        }
    }
    h
}

/// Returns the KDF params (if encrypted) and the header length.
pub fn read_header(r: &mut impl Read) -> Result<(Option<KdfParams>, u64)> {
    let mut m = [0u8; 9];
    r.read_exact(&mut m)?;
    if &m[..8] != MAGIC {
        bail!("not a dalzip archive");
    }
    match m[8] {
        0 => Ok((None, 9)),
        FLAG_ENCRYPTED => {
            let mut b = [0u8; 28];
            r.read_exact(&mut b)?;
            let u = |i: usize| u32::from_le_bytes(b[i..i + 4].try_into().unwrap());
            let kdf = KdfParams { salt: b[..16].try_into().unwrap(), m_kib: u(16), t: u(20), p: u(24) };
            Ok((Some(kdf), 37))
        }
        f => bail!("unknown header flags {f:#x}"),
    }
}
