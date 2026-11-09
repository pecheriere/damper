//! Memory pressure, availability and swap, from `sysctl`.

use std::io;

use damper_core::broker::MemorySample;
use damper_proto::Pressure;
use sysctl::{Ctl, CtlValue, Sysctl};

use crate::sources::MemorySource;

/// Size of `xsw_usage`: three `u64`s, a `u32` page size and a flag word.
const XSW_USAGE_LEN: usize = 32;
/// Offset of `xsu_used` in `xsw_usage` (after `xsu_total` and `xsu_avail`).
const XSW_USED_OFFSET: usize = 16;

/// Reads memory state through `sysctl`.
///
/// Uses `kern.memorystatus_vm_pressure_level` (1 normal, 2 warning, 4
/// critical), `kern.memorystatus_level` (percent available), `hw.memsize` and
/// `vm.swapusage`. All are readable without privileges.
#[derive(Debug, Clone, Copy, Default)]
pub struct SysctlMemory;

/// Maps `kern.memorystatus_vm_pressure_level` to a [`Pressure`].
#[must_use]
pub fn pressure_from_level(level: i32) -> Pressure {
    match level {
        4 => Pressure::Critical,
        2 => Pressure::Warning,
        _ => Pressure::Normal,
    }
}

/// Reads `xsu_used` from the bytes of an `xsw_usage` struct.
///
/// # Errors
///
/// Returns an error when the buffer is too short to hold the field.
pub fn swap_used_from_xsw(bytes: &[u8]) -> io::Result<u64> {
    bytes
        .get(XSW_USED_OFFSET..XSW_USED_OFFSET + 8)
        .and_then(|field| <[u8; 8]>::try_from(field).ok())
        .map(u64::from_ne_bytes)
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "vm.swapusage returned {} bytes, expected {XSW_USAGE_LEN}",
                    bytes.len()
                ),
            )
        })
}

fn clamp_pct(level: i64) -> u8 {
    u8::try_from(level.clamp(0, 100)).unwrap_or(100)
}

fn read(name: &str) -> io::Result<CtlValue> {
    let ctl = Ctl::new(name).map_err(|e| io::Error::other(format!("sysctl {name}: {e}")))?;
    ctl.value()
        .map_err(|e| io::Error::other(format!("sysctl {name}: {e}")))
}

fn unexpected(name: &str, value: &CtlValue) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        format!("sysctl {name}: unexpected value {value:?}"),
    )
}

fn read_int(name: &str) -> io::Result<i64> {
    let too_large = || io::Error::other(format!("sysctl {name}: value too large"));
    match read(name)? {
        CtlValue::Int(v) => Ok(i64::from(v)),
        CtlValue::Uint(v) => Ok(i64::from(v)),
        CtlValue::Long(v) | CtlValue::S64(v) => Ok(v),
        CtlValue::Ulong(v) | CtlValue::U64(v) => i64::try_from(v).map_err(|_| too_large()),
        other => Err(unexpected(name, &other)),
    }
}

fn read_u64(name: &str) -> io::Result<u64> {
    u64::try_from(read_int(name)?).map_err(|_| unexpected(name, &CtlValue::None))
}

impl MemorySource for SysctlMemory {
    fn sample(&self) -> io::Result<MemorySample> {
        let level = read_int("kern.memorystatus_vm_pressure_level")?;
        let pressure = pressure_from_level(i32::try_from(level).unwrap_or(0));
        let available_pct = clamp_pct(read_int("kern.memorystatus_level")?);
        let physical = read_u64("hw.memsize")?;
        let swap_used = match read("vm.swapusage")? {
            CtlValue::Struct(bytes) => swap_used_from_xsw(&bytes)?,
            other => return Err(unexpected("vm.swapusage", &other)),
        };
        Ok(MemorySample {
            pressure,
            available_pct,
            physical,
            swap_used,
        })
    }
}

#[cfg(test)]
#[expect(clippy::unwrap_used, reason = "test")]
mod tests {
    use super::*;

    #[test]
    fn pressure_levels_map() {
        assert_eq!(pressure_from_level(1), Pressure::Normal);
        assert_eq!(pressure_from_level(2), Pressure::Warning);
        assert_eq!(pressure_from_level(4), Pressure::Critical);
        assert_eq!(pressure_from_level(0), Pressure::Normal);
        assert_eq!(pressure_from_level(99), Pressure::Normal);
    }

    #[test]
    fn swap_used_parses_xsw_usage() {
        let mut buf = Vec::new();
        buf.extend_from_slice(&4_000_u64.to_ne_bytes());
        buf.extend_from_slice(&1_500_u64.to_ne_bytes());
        buf.extend_from_slice(&2_500_u64.to_ne_bytes());
        buf.extend_from_slice(&16_384_u32.to_ne_bytes());
        buf.extend_from_slice(&[1, 0, 0, 0]);
        assert_eq!(buf.len(), 32);
        assert_eq!(swap_used_from_xsw(&buf).unwrap(), 2_500);
    }

    #[test]
    fn swap_used_rejects_short_buffers() {
        assert!(swap_used_from_xsw(&[0; 23]).is_err());
    }

    #[test]
    fn available_percentage_is_clamped() {
        assert_eq!(clamp_pct(-5), 0);
        assert_eq!(clamp_pct(57), 57);
        assert_eq!(clamp_pct(250), 100);
    }

    #[test]
    fn sysctl_memory_reads_live_values() {
        let sample = SysctlMemory.sample().unwrap();
        assert!(sample.physical > 0);
        assert!(sample.available_pct <= 100);
    }
}
