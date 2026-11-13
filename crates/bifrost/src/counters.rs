//! Shows the counters of a VM instance's VM (see [`midgard_protocol::control::Counters`]).

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::io::{IsTerminal, Write};
use std::time::{Duration, Instant};

use anyhow::Result;
use midgard_protocol::control::Counters;

use crate::cli::CountersArgs;
use crate::client;

/// A group of counters that is shown under a heading.
type Section = (String, BTreeMap<String, u64>);

/// Prints the counters of the VM of `name` once, or (with `--watch`) again and again
/// with their rates since the last time.
pub fn show(name: &str, args: &CountersArgs) -> Result<()> {
    let mut previous = (Instant::now(), client::counters(name)?);
    let Some(interval) = args.watch else {
        print!("{}", render(&previous.1, None, args));
        return Ok(());
    };
    let clear = std::io::stdout().is_terminal();
    // The first time without rates.
    let mut text = render(&previous.1, None, args);
    loop {
        {
            let mut stdout = std::io::stdout().lock();
            if clear {
                // Like watch(1): from the top of a cleared screen.
                write!(stdout, "\x1b[H\x1b[2J")?;
            }
            write!(stdout, "{text}")?;
            stdout.flush()?;
        }
        std::thread::sleep(interval);
        let current = (Instant::now(), client::counters(name)?);
        let elapsed = current.0 - previous.0;
        text = render(&current.1, Some((&previous.1, elapsed)), args);
        if !clear {
            text.insert(0, '\n');
        }
        previous = current;
    }
}

/// The counters by section: the vCPUs' (summed up, or each vCPU's) and the devices'.
fn sections(snapshot: &Counters, per_vcpu: bool) -> Vec<Section> {
    let mut sections = Vec::new();
    if per_vcpu {
        for (index, counters) in snapshot.vcpus.iter().enumerate() {
            sections.push((format!("vCPU {index}"), counters.clone()));
        }
    } else {
        let mut sum = BTreeMap::<String, u64>::new();
        for counters in &snapshot.vcpus {
            for (name, value) in counters {
                *sum.entry(name.clone()).or_default() += value;
            }
        }
        let heading = match snapshot.vcpus.len() {
            1 => "vCPU".to_string(),
            count => format!("vCPUs (all {count})"),
        };
        sections.push((heading, sum));
    }
    sections.push(("devices".to_string(), snapshot.devices.clone()));
    sections
}

/// The text that shows the counters of `snapshot`; with `previous` (and the time since),
/// also their rates per second.
fn render(
    snapshot: &Counters,
    previous: Option<(&Counters, Duration)>,
    args: &CountersArgs,
) -> String {
    let current = sections(snapshot, args.per_vcpu);
    let previous = previous.map(|(snapshot, elapsed)| (sections(snapshot, args.per_vcpu), elapsed));
    let rows = |counters: &BTreeMap<String, u64>| {
        counters
            .iter()
            .filter(|&(_, &value)| args.all || value != 0)
            .map(|(name, &value)| (name.clone(), value))
            .collect::<Vec<_>>()
    };
    let width = current
        .iter()
        .flat_map(|(_, counters)| rows(counters))
        .map(|(name, _)| name.len())
        .max()
        .unwrap_or(0);
    let mut text = String::new();
    for (index, (heading, counters)) in current.iter().enumerate() {
        let rows = rows(counters);
        if rows.is_empty() {
            continue;
        }
        if !text.is_empty() {
            text.push('\n');
        }
        let _ = writeln!(text, "{heading}");
        // The same section of the previous counters (the vCPUs do not change).
        let before = previous
            .as_ref()
            .and_then(|(sections, elapsed)| Some((&sections.get(index)?.1, *elapsed)));
        for (name, value) in rows {
            let _ = write!(text, "  {name:<width$}  {value:>14}");
            if let Some((before, elapsed)) = before {
                let delta = value.saturating_sub(before.get(&name).copied().unwrap_or(0));
                let rate = delta as f64 / elapsed.as_secs_f64();
                let _ = write!(text, "  {rate:>12.0}/s");
            }
            text.push('\n');
        }
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(per_vcpu: bool, all: bool) -> CountersArgs {
        CountersArgs {
            name: None,
            per_vcpu,
            all,
            watch: None,
        }
    }

    fn snapshot(io: u64) -> Counters {
        let vcpu = |io: u64| BTreeMap::from([("exit.io".into(), io), ("exit.mmio".into(), 0)]);
        Counters {
            vcpus: vec![vcpu(io), vcpu(1)],
            devices: BTreeMap::from([("serial.pio_writes".into(), 3)]),
        }
    }

    #[test]
    fn sums_the_vcpus() {
        let text = render(&snapshot(2), None, &args(false, false));
        assert_eq!(
            text,
            "vCPUs (all 2)\n  exit.io                         3\n\n\
             devices\n  serial.pio_writes               3\n"
        );
    }

    #[test]
    fn shows_each_vcpu_and_zeros() {
        let text = render(&snapshot(2), None, &args(true, true));
        assert!(text.starts_with("vCPU 0\n  exit.io  "));
        assert!(text.contains("vCPU 1\n"));
        assert!(text.contains("  exit.mmio  "));
    }

    #[test]
    fn shows_rates() {
        let previous = snapshot(2);
        let text = render(
            &snapshot(12),
            Some((&previous, Duration::from_secs(2))),
            &args(false, false),
        );
        assert!(text.contains("exit.io                        13             5/s\n"));
    }
}
