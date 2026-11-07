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

pub fn now() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64)
}

impl Local {
    /// `YYYY-MM-DDThh:mm:ss`, as `.trashinfo` files want.
    pub fn rfc3339_local(&self) -> String {
        format!("{:04}-{:02}-{:02}T{:02}:{:02}:{:02}", self.year, self.month, self.day, self.hour, self.minute, self.second)
    }
}
