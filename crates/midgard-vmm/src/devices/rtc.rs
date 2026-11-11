//! An MC146818-compatible CMOS RTC (an index and a data port) that reads the host's wall
//! clock. There are no alarms or periodic interrupts, and the guest cannot set the time.

use std::time::Duration;

use crate::clock;

pub(crate) const NUM_PORTS: u16 = 2;
const INDEX_PORT: u16 = 0;
const DATA_PORT: u16 = 1;

const REG_SECONDS: u8 = 0x00;
const REG_MINUTES: u8 = 0x02;
const REG_HOURS: u8 = 0x04;
const REG_WEEKDAY: u8 = 0x06;
const REG_DAY: u8 = 0x07;
const REG_MONTH: u8 = 0x08;
const REG_YEAR: u8 = 0x09;
const REG_A: u8 = 0x0A;
const REG_B: u8 = 0x0B;
const REG_C: u8 = 0x0C;
const REG_D: u8 = 0x0D;
/// Not part of the MC146818; the FADT points the guest to it.
pub(crate) const REG_CENTURY: u8 = 0x32;

/// 32.768 kHz time base, 1024 Hz rate.
const REG_A_DEFAULT: u8 = 0x26;
/// Update in progress: set for the last [`UIP_NS`] of each second. Some guests wait for
/// it to clear before they read the time.
const REG_A_UIP: u8 = 1 << 7;
/// How long an update takes (eight cycles of the 32.768 kHz time base, as on the
/// MC146818); the time registers do not change for at least this long after UIP clears.
const UIP_NS: u32 = 244_141;
const REG_B_24H: u8 = 1 << 1;
const REG_B_BINARY: u8 = 1 << 2;
/// The bits of B that we keep: daylight saving (which has no effect), 24-hour mode,
/// binary mode and the square wave (whose output goes nowhere). The interrupt enables and
/// SET are dropped.
const REG_B_WRITABLE: u8 = 0x0F;
/// Hours register: PM in 12-hour mode.
const HOURS_PM: u8 = 1 << 7;
/// Valid RAM and time (the battery is fine).
const REG_D_VALID: u8 = 1 << 7;
/// Index port: disables NMIs (we have none).
const INDEX_NMI_DISABLE: u8 = 1 << 7;

pub(crate) struct Rtc {
    index: u8,
    /// The CMOS RAM, including registers A, B and D (but not the time registers).
    ram: [u8; 128],
}

impl Default for Rtc {
    fn default() -> Self {
        let mut ram = [0u8; 128];
        ram[REG_A as usize] = REG_A_DEFAULT;
        ram[REG_B as usize] = REG_B_24H;
        ram[REG_D as usize] = REG_D_VALID;
        Self { index: 0, ram }
    }
}

/// Converts days since 1970-01-01 to (year, month, day) (Howard Hinnant's
/// `civil_from_days`).
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let year = yoe + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

fn is_time_register(reg: u8) -> bool {
    matches!(
        reg,
        REG_SECONDS
            | REG_MINUTES
            | REG_HOURS
            | REG_WEEKDAY
            | REG_DAY
            | REG_MONTH
            | REG_YEAR
            | REG_CENTURY
    )
}

impl Rtc {
    /// Reads the port at `offset`.
    pub(crate) fn read(&self, offset: u16) -> u8 {
        match offset {
            // The index port is write-only.
            INDEX_PORT => 0xFF,
            DATA_PORT => self.read_register(self.index),
            _ => unreachable!(),
        }
    }

    /// Writes the port at `offset`.
    pub(crate) fn write(&mut self, offset: u16, value: u8) {
        match offset {
            INDEX_PORT => self.index = value & !INDEX_NMI_DISABLE,
            DATA_PORT => self.write_register(self.index, value),
            _ => unreachable!(),
        }
    }

    fn read_register(&self, reg: u8) -> u8 {
        match reg {
            reg if is_time_register(reg) => self.time_register(reg, clock::unix_time()),
            REG_A => self.ram[REG_A as usize] | update_in_progress(clock::unix_time()),
            // Reading C acknowledges interrupts (there are none).
            REG_C => 0,
            reg => self.ram[reg as usize],
        }
    }

    fn write_register(&mut self, reg: u8, value: u8) {
        match reg {
            reg if is_time_register(reg) => {}
            REG_A => self.ram[REG_A as usize] = value & !REG_A_UIP,
            REG_B => self.ram[REG_B as usize] = value & REG_B_WRITABLE,
            REG_C | REG_D => {}
            reg => self.ram[reg as usize] = value,
        }
    }

    /// The value of a time register at `now` (since the Unix epoch), in the format that
    /// register B selects.
    fn time_register(&self, reg: u8, now: Duration) -> u8 {
        let days = (now.as_secs() / 86_400) as i64;
        let seconds = (now.as_secs() % 86_400) as u32;
        let hours = seconds / 3600;
        let (year, month, day) = civil_from_days(days);
        let value = match reg {
            REG_SECONDS => seconds % 60,
            REG_MINUTES => seconds / 60 % 60,
            REG_HOURS if self.ram[REG_B as usize] & REG_B_24H != 0 => hours,
            REG_HOURS => {
                let hour12 = if hours.is_multiple_of(12) {
                    12
                } else {
                    hours % 12
                };
                let pm = if hours >= 12 { HOURS_PM } else { 0 };
                return self.encode(hour12) | pm;
            }
            // 1970-01-01 was a Thursday; Sunday is 1.
            REG_WEEKDAY => ((days + 4).rem_euclid(7) + 1) as u32,
            REG_DAY => day,
            REG_MONTH => month,
            REG_YEAR => year.rem_euclid(100) as u32,
            REG_CENTURY => year.div_euclid(100) as u32,
            _ => unreachable!(),
        };
        self.encode(value)
    }

    /// Encodes `value` (below 100) in binary or BCD, as register B selects.
    fn encode(&self, value: u32) -> u8 {
        if self.ram[REG_B as usize] & REG_B_BINARY != 0 {
            value as u8
        } else {
            (((value / 10) << 4) | (value % 10)) as u8
        }
    }
}

/// Register A's UIP bit at `now`.
fn update_in_progress(now: Duration) -> u8 {
    if now.subsec_nanos() >= 1_000_000_000 - UIP_NS {
        REG_A_UIP
    } else {
        0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dates() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(19_358), (2023, 1, 1));
        assert_eq!(civil_from_days(19_782), (2024, 2, 29));
        assert_eq!(civil_from_days(-1), (1969, 12, 31));
    }

    #[test]
    fn time() {
        // 2024-02-29 (a Thursday) 13:05:09.
        let at = |hours: u64| Duration::from_secs(19_782 * 86_400 + hours * 3600 + 5 * 60 + 9);
        let now = at(13);
        let mut rtc = Rtc::default();
        let read = |rtc: &Rtc, reg| rtc.time_register(reg, now);
        // BCD and 24-hour mode by default.
        assert_eq!(read(&rtc, REG_SECONDS), 0x09);
        assert_eq!(read(&rtc, REG_MINUTES), 0x05);
        assert_eq!(read(&rtc, REG_HOURS), 0x13);
        assert_eq!(read(&rtc, REG_WEEKDAY), 0x05);
        assert_eq!(read(&rtc, REG_DAY), 0x29);
        assert_eq!(read(&rtc, REG_MONTH), 0x02);
        assert_eq!(read(&rtc, REG_YEAR), 0x24);
        assert_eq!(read(&rtc, REG_CENTURY), 0x20);
        // BCD, 12-hour mode.
        rtc.write_register(REG_B, 0);
        assert_eq!(rtc.time_register(REG_HOURS, at(0)), 0x12);
        assert_eq!(rtc.time_register(REG_HOURS, at(12)), HOURS_PM | 0x12);
        assert_eq!(rtc.time_register(REG_HOURS, at(22)), HOURS_PM | 0x10);
        // Binary, 12-hour mode.
        rtc.write_register(REG_B, REG_B_BINARY);
        assert_eq!(read(&rtc, REG_DAY), 29);
        assert_eq!(rtc.time_register(REG_HOURS, at(22)), HOURS_PM | 10);
    }

    #[test]
    fn update_in_progress_at_end_of_second() {
        let at = |nanos| Duration::new(1_794_430_855, nanos);
        assert_eq!(update_in_progress(at(0)), 0);
        assert_eq!(update_in_progress(at(999_000_000)), 0);
        assert_eq!(update_in_progress(at(999_800_000)), REG_A_UIP);
    }

    #[test]
    fn registers() {
        let mut rtc = Rtc::default();
        let mut access = |reg, value: Option<u8>| {
            rtc.write(INDEX_PORT, reg | INDEX_NMI_DISABLE);
            if let Some(value) = value {
                rtc.write(DATA_PORT, value);
            }
            rtc.read(DATA_PORT)
        };
        assert_eq!(access(REG_D, None), REG_D_VALID);
        // SET and the interrupt enables are dropped.
        assert_eq!(access(REG_B, Some(0xFF)), REG_B_WRITABLE);
        // The time cannot be set.
        assert_ne!(access(REG_MONTH, Some(0x13)), 0x13);
        // CMOS RAM is writable.
        assert_eq!(access(0x40, Some(0x5A)), 0x5A);
        // The update-in-progress bit cannot be written.
        rtc.write_register(REG_A, 0xFF);
        assert_eq!(rtc.ram[REG_A as usize], !REG_A_UIP);
    }
}
