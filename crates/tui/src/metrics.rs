use std::{collections::VecDeque, time::Duration};

use sysinfo::{CpuRefreshKind, MemoryRefreshKind, RefreshKind, System};

const HISTORY_DURATION: Duration = Duration::from_secs(15 * 60);

#[derive(Debug, Default)]
pub(crate) struct HostMetrics {
    pub cpu_percent: u64,
    pub memory_percent: u64,
    cpu_history: VecDeque<(std::time::Instant, u64)>,
}

impl HostMetrics {
    pub(crate) fn record(&mut self, cpu_percent: u64, memory_percent: u64) {
        self.record_at(cpu_percent, memory_percent, std::time::Instant::now());
    }

    fn record_at(&mut self, cpu_percent: u64, memory_percent: u64, now: std::time::Instant) {
        self.cpu_percent = cpu_percent.min(100);
        self.memory_percent = memory_percent.min(100);
        self.cpu_history.push_back((now, self.cpu_percent));
        while self
            .cpu_history
            .front()
            .is_some_and(|(sampled_at, _)| now.duration_since(*sampled_at) > HISTORY_DURATION)
        {
            self.cpu_history.pop_front();
        }
    }

    pub fn cpu_sparkline(&self, width: usize) -> Vec<u64> {
        if width == 0 {
            return Vec::new();
        }
        let values = self
            .cpu_history
            .iter()
            .map(|(_, value)| *value)
            .collect::<Vec<_>>();
        if values.len() <= width {
            let mut padded = vec![0; width - values.len()];
            padded.extend(values);
            return padded;
        }
        (0..width)
            .map(|column| {
                let start = column * values.len() / width;
                let end = ((column + 1) * values.len() / width).max(start + 1);
                values[start..end].iter().copied().max().unwrap_or(0)
            })
            .collect()
    }

    pub fn has_samples(&self) -> bool {
        !self.cpu_history.is_empty()
    }
}

pub(crate) struct HostMetricsSampler {
    system: System,
}

impl HostMetricsSampler {
    pub fn new() -> Self {
        Self {
            system: System::new_with_specifics(
                RefreshKind::nothing()
                    .with_cpu(CpuRefreshKind::everything())
                    .with_memory(MemoryRefreshKind::everything()),
            ),
        }
    }

    pub fn sample(&mut self, metrics: &mut HostMetrics) {
        self.system.refresh_cpu_usage();
        self.system.refresh_memory();
        let cpu = self.system.global_cpu_usage().round() as u64;
        let memory = self
            .system
            .used_memory()
            .saturating_mul(100)
            .checked_div(self.system.total_memory())
            .unwrap_or(0);
        metrics.record(cpu, memory);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn history_expires_after_fifteen_minutes_and_downsamples_peaks() {
        let start = std::time::Instant::now();
        let mut metrics = HostMetrics::default();
        metrics.record_at(10, 20, start);
        metrics.record_at(20, 30, start + Duration::from_secs(5));
        metrics.record_at(90, 40, start + Duration::from_secs(10));
        metrics.record_at(30, 50, start + Duration::from_secs(15));

        assert_eq!(metrics.cpu_sparkline(2), [20, 90]);

        metrics.record_at(40, 60, start + HISTORY_DURATION + Duration::from_secs(11));
        assert_eq!(metrics.cpu_sparkline(2), [30, 40]);
        assert_eq!(metrics.cpu_percent, 40);
        assert_eq!(metrics.memory_percent, 60);
    }

    #[test]
    fn short_history_is_right_aligned() {
        let mut metrics = HostMetrics::default();
        metrics.record(75, 50);
        assert_eq!(metrics.cpu_sparkline(4), [0, 0, 0, 75]);
    }
}
