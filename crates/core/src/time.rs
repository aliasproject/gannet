//! Local time, from the C library so it follows the system time zone.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Local {
    pub year: i32,
    pub month: u32,
    pub day: u32,
    pub hour: u32,
    pub minute: u32,
    pub second: u32,
}

pub fn local(seconds: i64) -> Local {
    // SAFETY: `tm` is plain data and localtime_r only writes to the given struct.
    let tm = unsafe {
        let mut tm: libc::tm = std::mem::zeroed();
        let t = seconds as libc::time_t;
        libc::localtime_r(&t, &mut tm);
        tm
    };
    Local { year: tm.tm_year + 1900, month: tm.tm_mon as u32 + 1, day: tm.tm_mday as u32, hour: tm.tm_hour as u32, minute: tm.tm_min as u32, second: tm.tm_sec as u32 }
}

/// Seconds since the epoch for a local time, or None if the C library can't place it.
pub fn from_local(t: Local) -> Option<i64> {
    // SAFETY: `tm` is plain data, and mktime only reads and normalizes the given struct.
    let secs = unsafe {
        let mut tm: libc::tm = std::mem::zeroed();
        tm.tm_year = t.year - 1900;
        tm.tm_mon = t.month as i32 - 1;
        tm.tm_mday = t.day as i32;
        tm.tm_hour = t.hour as i32;
        tm.tm_min = t.minute as i32;
        tm.tm_sec = t.second as i32;
        // Let the C library work out whether daylight saving applied then.
        tm.tm_isdst = -1;
        libc::mktime(&mut tm)
    };
    (secs != -1).then_some(secs as i64)
}

pub fn now() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64)
}

impl Local {
    /// `YYYY-MM-DDThh:mm:ss`, as `.trashinfo` files want.
    pub fn rfc3339_local(&self) -> String {
        format!("{:04}-{:02}-{:02}T{:02}:{:02}:{:02}", self.year, self.month, self.day, self.hour, self.minute, self.second)
    }

    /// Reads `YYYY-MM-DDThh:mm:ss`, as written by `rfc3339_local`.
    pub fn parse_rfc3339_local(text: &str) -> Option<Local> {
        let b = text.as_bytes();
        if b.len() < 19 || b[4] != b'-' || b[7] != b'-' || !matches!(b[10], b'T' | b' ') || b[13] != b':' || b[16] != b':' {
            return None;
        }
        let num = |r: std::ops::Range<usize>| text.get(r).filter(|s| s.bytes().all(|c| c.is_ascii_digit())).and_then(|s| s.parse::<u32>().ok());
        let t = Local { year: num(0..4)? as i32, month: num(5..7)?, day: num(8..10)?, hour: num(11..13)?, minute: num(14..16)?, second: num(17..19)? };
        let ok = (1..=12).contains(&t.month) && (1..=31).contains(&t.day) && t.hour < 24 && t.minute < 60 && t.second <= 60;
        ok.then_some(t)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trash_dates_read_back_as_written() {
        let t = local(1_700_000_000);
        let back = Local::parse_rfc3339_local(&t.rfc3339_local()).unwrap();
        assert_eq!(back, t);
        assert_eq!(from_local(back), Some(1_700_000_000));
    }

    #[test]
    fn bad_trash_dates_are_refused() {
        for bad in ["", "2026-11-10", "2026-13-01T00:00:00", "2026-11-10T25:00:00", "2026-1x-10T00:00:00", "20261110T000000"] {
            assert_eq!(Local::parse_rfc3339_local(bad), None, "{bad}");
        }
        assert!(Local::parse_rfc3339_local("2026-11-10T08:30:00+01:00").is_some(), "a zone suffix is ignored");
    }
}
