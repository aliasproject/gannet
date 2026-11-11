//! How sizes and dates read.

use gannet_core::time::{Local, local};

/// Decimal units, as GNOME and most of the desktop show them.
pub fn size(bytes: u64) -> String {
    const UNITS: [&str; 6] = ["kB", "MB", "GB", "TB", "PB", "EB"];
    if bytes < 1000 {
        return format!("{bytes} B");
    }
    let mut v = bytes as f64 / 1000.0;
    let mut u = 0;
    while v >= 999.95 && u + 1 < UNITS.len() {
        v /= 1000.0;
        u += 1;
    }
    if v < 100.0 {
        format!("{v:.1} {}", UNITS[u])
    } else {
        format!("{v:.0} {}", UNITS[u])
    }
}

const MONTHS: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];

/// `14:03` today, `Nov 3 14:03` this year, `Nov 3 2024` before that.
pub fn date(seconds: i64, now: i64) -> String {
    date_parts(local(seconds), local(now))
}

fn date_parts(t: Local, now: Local) -> String {
    let month = MONTHS[(t.month as usize).clamp(1, 12) - 1];
    if t.year != now.year {
        format!("{month} {} {}", t.day, t.year)
    } else if t.month == now.month && t.day == now.day {
        format!("{:02}:{:02}", t.hour, t.minute)
    } else {
        format!("{month} {} {:02}:{:02}", t.day, t.hour, t.minute)
    }
}

pub fn count(n: usize, one: &str, many: &str) -> String {
    let mut digits = n.to_string();
    let mut i = digits.len() as isize - 3;
    while i > 0 {
        digits.insert(i as usize, ',');
        i -= 3;
    }
    format!("{digits} {}", if n == 1 { one } else { many })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes_read_well() {
        assert_eq!(size(0), "0 B");
        assert_eq!(size(999), "999 B");
        assert_eq!(size(1000), "1.0 kB");
        assert_eq!(size(1_500_000), "1.5 MB");
        assert_eq!(size(250_000_000), "250 MB");
        assert_eq!(size(999_999), "1.0 MB");
        assert_eq!(size(u64::MAX), "18.4 EB");
    }

    fn t(year: i32, month: u32, day: u32, hour: u32, minute: u32) -> Local {
        Local {
            year,
            month,
            day,
            hour,
            minute,
            second: 0,
        }
    }

    #[test]
    fn dates_read_well() {
        let now = t(2026, 11, 3, 22, 0);
        assert_eq!(date_parts(t(2026, 11, 3, 14, 5), now), "14:05");
        assert_eq!(date_parts(t(2026, 3, 9, 8, 7), now), "Mar 9 08:07");
        assert_eq!(date_parts(t(2024, 12, 25, 8, 7), now), "Dec 25 2024");
    }

    #[test]
    fn counts_read_well() {
        assert_eq!(count(1, "item", "items"), "1 item");
        assert_eq!(count(100000, "item", "items"), "100,000 items");
        assert_eq!(count(1234567, "x", "xs"), "1,234,567 xs");
        assert_eq!(count(999, "x", "xs"), "999 xs");
    }
}
