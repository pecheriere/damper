//! Resolution of `config.toml` against the machine's size.
//!
//! [`load`] never fails. Each key is validated on its own: a bad key gets its
//! default and one warning that names it, and the other keys are kept.

use std::time::Duration;

use toml::{Table, Value};

use crate::units::{parse_bytes, parse_duration};

/// Default `memory` as a percentage of physical memory.
const DEFAULT_MEMORY_PERCENT: u128 = 75;
/// Default `max_hold` in seconds.
const DEFAULT_MAX_HOLD_SECS: u64 = 600;

const KNOWN_KEYS: [&str; 6] = [
    "cores",
    "memory",
    "max_hold",
    "enabled",
    "priority",
    "extra_build_subcommands",
];

/// How the shim lowers cargo's scheduling priority (spec §7.4).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Priority {
    /// Utility `QoS` class (`taskpolicy -c utility`).
    #[default]
    Utility,
    /// `nice`.
    Nice,
    /// No change.
    None,
}

/// The machine's size, as read by the caller.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Machine {
    /// Logical cores.
    pub cores: u32,
    /// Physical memory in bytes.
    pub memory: u64,
}

/// The configuration after defaults, percentages and clamping.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Effective {
    /// The most build jobs at once.
    pub cores: u32,
    /// The most memory builds may use, in bytes.
    pub memory: u64,
    /// The longest a build waits for a token before running anyway.
    pub max_hold: Duration,
    /// Whether damper is enabled by default.
    pub enabled: bool,
    /// Priority applied to cargo.
    pub priority: Priority,
    /// Cargo subcommands to treat as builds, in addition to the built-in set.
    pub extra_build_subcommands: Vec<String>,
}

/// The result of [`load`]: the effective config and what was wrong with the file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Loaded {
    /// The resolved settings.
    pub effective: Effective,
    /// One entry per problem, each naming the key.
    pub warnings: Vec<String>,
}

/// Resolves the config text (`None` when there is no file) against `machine`.
#[must_use]
pub fn load(text: Option<&str>, machine: Machine) -> Loaded {
    let mut warnings = Vec::new();
    let mut effective = defaults(machine);
    let Some(text) = text else {
        return Loaded {
            effective,
            warnings,
        };
    };
    let table = match text.parse::<Table>() {
        Ok(table) => table,
        Err(e) => {
            warnings.push(format!(
                "config is not valid TOML ({}); using defaults",
                e.message()
            ));
            return Loaded {
                effective,
                warnings,
            };
        }
    };
    for key in table.keys() {
        if !KNOWN_KEYS.contains(&key.as_str()) {
            warnings.push(format!("unknown key `{key}` ignored"));
        }
    }
    let mut ctx = Ctx {
        machine,
        warnings: &mut warnings,
    };
    if let Some(cores) = table.get("cores").and_then(|v| ctx.cores(v)) {
        effective.cores = cores;
    }
    if let Some(memory) = table.get("memory").and_then(|v| ctx.memory(v)) {
        effective.memory = memory;
    }
    if let Some(hold) = table.get("max_hold").and_then(|v| ctx.max_hold(v)) {
        effective.max_hold = hold;
    }
    if let Some(enabled) = table.get("enabled").and_then(|v| ctx.enabled(v)) {
        effective.enabled = enabled;
    }
    if let Some(priority) = table.get("priority").and_then(|v| ctx.priority(v)) {
        effective.priority = priority;
    }
    if let Some(list) = table
        .get("extra_build_subcommands")
        .and_then(|v| ctx.subcommands(v))
    {
        effective.extra_build_subcommands = list;
    }
    Loaded {
        effective,
        warnings,
    }
}

fn defaults(machine: Machine) -> Effective {
    Effective {
        cores: machine.cores,
        memory: percent_of(machine.memory, DEFAULT_MEMORY_PERCENT),
        max_hold: Duration::from_secs(DEFAULT_MAX_HOLD_SECS),
        enabled: true,
        priority: Priority::default(),
        extra_build_subcommands: Vec::new(),
    }
}

/// `value * pct / 100`, rounded down, without overflow for `pct <= 100`.
fn percent_of(value: u64, pct: u128) -> u64 {
    u64::try_from(u128::from(value) * pct / 100).unwrap_or(u64::MAX)
}

/// Parses `"NN%"` with `1 <= NN <= 100`. `None` when `s` is not a percentage.
fn parse_percent(s: &str) -> Option<Result<u128, ()>> {
    let digits = s.trim().strip_suffix('%')?;
    Some(match digits.trim().parse::<u8>() {
        Ok(p @ 1..=100) => Ok(u128::from(p)),
        _ => Err(()),
    })
}

struct Ctx<'a> {
    machine: Machine,
    warnings: &'a mut Vec<String>,
}

impl Ctx<'_> {
    fn invalid<T>(&mut self, key: &str, expected: &str) -> Option<T> {
        self.warnings.push(format!(
            "`{key}` is invalid (expected {expected}); using the default"
        ));
        None
    }

    fn clamped(&mut self, key: &str, what: &str) {
        self.warnings.push(format!(
            "`{key}` exceeds the machine's {what}; clamped to it"
        ));
    }

    fn cores(&mut self, v: &Value) -> Option<u32> {
        const EXPECTED: &str = "a positive integer or a percentage such as \"50%\"";
        let max = self.machine.cores;
        let requested = match v {
            Value::Integer(n) => match u64::try_from(*n) {
                Ok(n) if n > 0 => n,
                _ => return self.invalid("cores", EXPECTED),
            },
            Value::String(s) => match parse_percent(s) {
                Some(Ok(p)) => {
                    let n = (u128::from(max) * p / 100).max(1);
                    return Some(u32::try_from(n).unwrap_or(max));
                }
                _ => return self.invalid("cores", EXPECTED),
            },
            _ => return self.invalid("cores", EXPECTED),
        };
        if requested > u64::from(max) {
            self.clamped("cores", "core count");
            return Some(max);
        }
        u32::try_from(requested).ok()
    }

    fn memory(&mut self, v: &Value) -> Option<u64> {
        const EXPECTED: &str = "a size such as \"24GiB\" or a percentage such as \"50%\"";
        let max = self.machine.memory;
        let Value::String(s) = v else {
            return self.invalid("memory", EXPECTED);
        };
        let requested = match parse_percent(s) {
            Some(Ok(p)) => return Some(percent_of(max, p)),
            Some(Err(())) => return self.invalid("memory", EXPECTED),
            None => match parse_bytes(s) {
                Ok(n) if n > 0 => n,
                _ => return self.invalid("memory", EXPECTED),
            },
        };
        if requested > max {
            self.clamped("memory", "physical memory");
            return Some(max);
        }
        Some(requested)
    }

    fn max_hold(&mut self, v: &Value) -> Option<Duration> {
        const EXPECTED: &str = "a positive duration such as \"10m\"";
        match v {
            Value::String(s) => match parse_duration(s) {
                Ok(d) if !d.is_zero() => Some(d),
                _ => self.invalid("max_hold", EXPECTED),
            },
            _ => self.invalid("max_hold", EXPECTED),
        }
    }

    fn enabled(&mut self, v: &Value) -> Option<bool> {
        if let Value::Boolean(b) = v {
            Some(*b)
        } else {
            self.invalid("enabled", "true or false")
        }
    }

    fn priority(&mut self, v: &Value) -> Option<Priority> {
        match v.as_str() {
            Some("utility") => Some(Priority::Utility),
            Some("nice") => Some(Priority::Nice),
            Some("none") => Some(Priority::None),
            _ => self.invalid("priority", "\"utility\", \"nice\" or \"none\""),
        }
    }

    fn subcommands(&mut self, v: &Value) -> Option<Vec<String>> {
        const EXPECTED: &str = "a list of non-empty subcommand names without whitespace";
        let Value::Array(items) = v else {
            return self.invalid("extra_build_subcommands", EXPECTED);
        };
        let mut out = Vec::with_capacity(items.len());
        for item in items {
            match item.as_str() {
                Some(s) if !s.is_empty() && !s.contains(char::is_whitespace) => {
                    out.push(s.to_owned());
                }
                _ => return self.invalid("extra_build_subcommands", EXPECTED),
            }
        }
        Some(out)
    }
}

#[cfg(test)]
mod tests;
