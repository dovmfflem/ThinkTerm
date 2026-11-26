//! Resource counters shared by the local and SSH status collectors.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CpuTimes {
    pub total: u64,
    pub idle: u64,
}

impl CpuTimes {
    pub fn busy_percent_since(self, previous: Self) -> Option<u64> {
        let total = self.total.checked_sub(previous.total)?;
        let idle = self.idle.checked_sub(previous.idle)?;
        let busy = total.checked_sub(idle)?;
        (busy as u128 * 100 + total as u128 / 2)
            .checked_div(total as u128)
            .map(|percent| percent as u64)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Capacity {
    pub used: u64,
    pub total: u64,
}

impl Capacity {
    pub fn new(used: u64, total: u64) -> Option<Self> {
        (total > 0 && used <= total).then_some(Self { used, total })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GpuMemory {
    pub index: u32,
    pub bytes: Capacity,
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct ResourceSample {
    pub cpu: Option<CpuTimes>,
    pub ram: Option<Capacity>,
    pub disk: Option<Capacity>,
    pub gpus: Vec<GpuMemory>,
}

impl ResourceSample {
    pub fn has_data(&self) -> bool {
        self.cpu.is_some() || self.ram.is_some() || self.disk.is_some() || !self.gpus.is_empty()
    }

    pub fn parse_linux(stdout: &str) -> Self {
        let mut sample = Self::default();
        for line in stdout.lines() {
            let mut fields = line.split_ascii_whitespace();
            match fields.next() {
                Some("cpu") => sample.cpu = parse_linux_cpu(fields),
                Some("ram") => {
                    let mut counts = fields.map(str::parse::<u64>);
                    if let (Some(Ok(total)), Some(Ok(available))) = (counts.next(), counts.next()) {
                        sample.ram = total
                            .checked_sub(available)
                            .and_then(|used| capacity_in_units(used, total, 1024));
                    }
                }
                Some("disk") => {
                    let mut counts = fields.map(str::parse::<u64>);
                    if let (Some(Ok(total)), Some(Ok(used))) = (counts.next(), counts.next()) {
                        sample.disk = capacity_in_units(used, total, 1024);
                    }
                }
                _ => {}
            }
        }
        sample
    }
}

fn parse_linux_cpu<'a>(fields: impl Iterator<Item = &'a str>) -> Option<CpuTimes> {
    let mut total = 0u64;
    let mut idle = 0u64;
    let mut count = 0;
    // guest and guest_nice, if present, are already included in user/nice.
    for (index, field) in fields.take(8).enumerate() {
        let ticks = field.parse::<u64>().ok()?;
        total = total.checked_add(ticks)?;
        if index == 3 || index == 4 {
            idle = idle.checked_add(ticks)?;
        }
        count += 1;
    }
    (count >= 4).then_some(CpuTimes { total, idle })
}

fn capacity_in_units(used: u64, total: u64, bytes_per_unit: u64) -> Option<Capacity> {
    Capacity::new(
        used.checked_mul(bytes_per_unit)?,
        total.checked_mul(bytes_per_unit)?,
    )
}

pub fn parse_nvidia_smi(stdout: &str) -> Vec<GpuMemory> {
    let mut gpus: Vec<_> = stdout
        .lines()
        .filter_map(|line| {
            let mut fields = line.split(',').map(str::trim);
            let index = fields.next()?.parse().ok()?;
            let used = fields.next()?.parse().ok()?;
            let total = fields.next()?.parse().ok()?;
            Some(GpuMemory {
                index,
                bytes: capacity_in_units(used, total, 1024 * 1024)?,
            })
        })
        .collect();
    gpus.sort_unstable_by_key(|gpu| gpu.index);
    gpus
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn linux_sample_tracks_deltas_and_ignores_double_counted_guest_ticks() {
        let first = ResourceSample::parse_linux(
            "cpu 100 0 50 800 50 0 0 0 200 0\nram 8000 3000\ndisk 20000 5000\n",
        );
        let second = ResourceSample::parse_linux("cpu 150 0 75 920 55 0 0 0 300 0\n");
        let before = first.cpu.unwrap();
        let after = second.cpu.unwrap();
        assert_eq!(after.busy_percent_since(before), Some(38));
        assert_eq!(before.busy_percent_since(after), None);
        assert_eq!(after.busy_percent_since(after), None);
        assert_eq!(first.ram, Capacity::new(5_000 * 1024, 8_000 * 1024));
        assert_eq!(first.disk, Capacity::new(5_000 * 1024, 20_000 * 1024));
    }

    #[test]
    fn malformed_rows_do_not_turn_into_zero_usage() {
        let sample = ResourceSample::parse_linux("cpu 1 2 bad\nram 10 11\ndisk 0 0\n");
        assert!(!sample.has_data());
        assert!(parse_nvidia_smi("Failed to initialize NVML\n").is_empty());
    }

    #[test]
    fn nvidia_rows_are_independent_and_sorted_by_device_index() {
        let gpus = parse_nvidia_smi("2, [N/A], 24576\r\n1, 1024, 24576\r\n0, 0, 8192\r\n");
        assert_eq!(gpus.iter().map(|gpu| gpu.index).collect::<Vec<_>>(), [0, 1]);
        assert_eq!(gpus[0].bytes.used, 0);
        assert_eq!(gpus[1].bytes.total, 24 * 1024 * 1024 * 1024);
    }
}
