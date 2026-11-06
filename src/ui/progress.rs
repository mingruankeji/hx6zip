use std::time::{Duration, Instant};

use mzip::archive::Progress;

pub(super) struct CompressionTiming {
    started_at: Instant,
    paused_at: Option<Instant>,
    pauses: Vec<(Instant, Instant)>,
    finished_at: Option<Instant>,
    completed: bool,
}

pub(super) struct CompressionMetrics {
    pub elapsed: Duration,
    pub speed_mib_per_second: Option<f64>,
    pub remaining: Option<Duration>,
    pub ratio_percent: Option<f64>,
}

impl CompressionTiming {
    pub fn new(started_at: Instant) -> Self {
        Self {
            started_at,
            paused_at: None,
            pauses: Vec::new(),
            finished_at: None,
            completed: false,
        }
    }

    pub fn set_paused(&mut self, paused: bool, now: Instant) {
        if paused {
            if self.paused_at.is_none() {
                self.paused_at = Some(now);
            }
        } else if let Some(paused_at) = self.paused_at.take() {
            if now > paused_at {
                self.pauses.push((paused_at, now));
            }
        }
    }

    pub fn finish(&mut self, now: Instant, completed: bool) {
        self.set_paused(false, now);
        self.finished_at = Some(now);
        self.completed = completed;
    }

    pub fn completed(&self) -> bool {
        self.completed
    }

    pub fn metrics(
        &self,
        progress: Option<&Progress>,
        now: Instant,
        cancelled: bool,
    ) -> CompressionMetrics {
        let end = self.finished_at.unwrap_or(now);
        let elapsed = end.saturating_duration_since(self.started_at);
        let paused_duration = self
            .pauses
            .iter()
            .fold(Duration::ZERO, |total, (start, stop)| {
                total.saturating_add(end.min(*stop).saturating_duration_since(*start))
            });
        let paused_duration = paused_duration.saturating_add(
            self.paused_at
                .map_or(Duration::ZERO, |start| end.saturating_duration_since(start)),
        );
        let active = elapsed.saturating_sub(paused_duration);
        let can_estimate = self.paused_at.is_none()
            && !cancelled
            && !active.is_zero()
            && (active >= Duration::from_secs(1) || self.completed);

        let speed_mib_per_second = progress
            .filter(|progress| can_estimate && progress.completed_bytes > 0)
            .map(|progress| {
                progress.completed_bytes as f64 / (1024.0 * 1024.0) / active.as_secs_f64()
            });
        let remaining = if self.completed {
            Some(Duration::ZERO)
        } else {
            progress
                .filter(|progress| {
                    can_estimate
                        && progress.completed_bytes > 0
                        && progress.completed_bytes < progress.total_bytes
                })
                .and_then(|progress| {
                    let seconds = active.as_secs_f64()
                        * (progress.total_bytes - progress.completed_bytes) as f64
                        / progress.completed_bytes as f64;
                    Duration::try_from_secs_f64(seconds.ceil().max(1.0)).ok()
                })
        };
        let ratio_percent = progress
            .filter(|progress| progress.completed_bytes > 0)
            .and_then(|progress| {
                progress
                    .archive_size
                    .map(|size| 100.0 * size as f64 / progress.completed_bytes as f64)
            });

        CompressionMetrics {
            elapsed,
            speed_mib_per_second,
            remaining,
            ratio_percent,
        }
    }
}

pub(super) fn is_verification(operation: &str) -> bool {
    matches!(
        operation,
        "正在校验压缩包" | "正在校验解压结果" | "正在提交解压结果"
    )
}

pub(super) fn format_duration(duration: Duration) -> String {
    let seconds = duration.as_secs();
    format!(
        "{:02}:{:02}:{:02}",
        seconds / 3600,
        seconds / 60 % 60,
        seconds % 60
    )
}

pub(super) fn countdown_seconds(deadline: Instant, now: Instant) -> u64 {
    let remaining = deadline.saturating_duration_since(now);
    remaining.as_secs() + u64::from(remaining.subsec_nanos() != 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    const MIB: u64 = 1024 * 1024;

    #[test]
    fn close_countdown_counts_down_for_five_full_seconds() {
        let start = Instant::now();
        let deadline = start + Duration::from_secs(5);
        assert_eq!(countdown_seconds(deadline, start), 5);
        assert_eq!(
            countdown_seconds(deadline, start + Duration::from_millis(1001)),
            4
        );
        assert_eq!(
            countdown_seconds(deadline, start + Duration::from_millis(4999)),
            1
        );
        assert_eq!(countdown_seconds(deadline, deadline), 0);
    }

    fn progress(completed: u64, total: u64, archive_size: u64) -> Progress {
        Progress {
            operation: "正在压缩",
            current_file: "source".to_owned(),
            completed_bytes: completed,
            total_bytes: total,
            archive_size: Some(archive_size),
        }
    }

    #[test]
    fn estimates_use_active_time_and_freeze_after_completion() {
        let start = Instant::now();
        let mut timing = CompressionTiming::new(start);
        let first = progress(4 * MIB, 12 * MIB, 2 * MIB);
        let metrics = timing.metrics(Some(&first), start + Duration::from_secs(4), false);
        assert_eq!(metrics.elapsed, Duration::from_secs(4));
        assert_eq!(metrics.speed_mib_per_second, Some(1.0));
        assert_eq!(metrics.remaining, Some(Duration::from_secs(8)));
        assert_eq!(metrics.ratio_percent, Some(50.0));

        timing.set_paused(true, start + Duration::from_secs(4));
        let paused = timing.metrics(Some(&first), start + Duration::from_secs(9), false);
        assert_eq!(paused.elapsed, Duration::from_secs(9));
        assert_eq!(paused.speed_mib_per_second, None);
        assert_eq!(paused.remaining, None);

        timing.set_paused(false, start + Duration::from_secs(14));
        let resumed = progress(6 * MIB, 12 * MIB, 3 * MIB);
        let metrics = timing.metrics(Some(&resumed), start + Duration::from_secs(16), false);
        assert_eq!(metrics.elapsed, Duration::from_secs(16));
        assert_eq!(metrics.speed_mib_per_second, Some(1.0));
        assert_eq!(metrics.remaining, Some(Duration::from_secs(6)));

        let finished_input = progress(12 * MIB, 12 * MIB, 6 * MIB);
        assert_eq!(
            timing
                .metrics(
                    Some(&finished_input),
                    start + Duration::from_secs(16),
                    false
                )
                .remaining,
            None
        );
        timing.finish(start + Duration::from_secs(16), true);
        let finished = timing.metrics(
            Some(&finished_input),
            start + Duration::from_secs(30),
            false,
        );
        assert_eq!(finished.elapsed, Duration::from_secs(16));
        assert_eq!(finished.speed_mib_per_second, Some(2.0));
        assert_eq!(finished.remaining, Some(Duration::ZERO));
        assert_eq!(finished.ratio_percent, Some(50.0));
    }

    #[test]
    fn handles_empty_inputs_cancellation_and_expanding_archives() {
        let start = Instant::now();
        let mut timing = CompressionTiming::new(start);
        let empty = progress(0, 0, 100);
        let empty_metrics = timing.metrics(Some(&empty), start + Duration::from_secs(3), false);
        assert_eq!(empty_metrics.speed_mib_per_second, None);
        assert_eq!(empty_metrics.remaining, None);
        assert_eq!(empty_metrics.ratio_percent, None);

        let expanded = progress(1, 2, 3);
        let metrics = timing.metrics(Some(&expanded), start + Duration::from_secs(3), true);
        assert_eq!(metrics.speed_mib_per_second, None);
        assert_eq!(metrics.remaining, None);
        assert_eq!(metrics.ratio_percent, Some(300.0));

        timing.finish(start + Duration::from_secs(3), true);
        assert_eq!(
            timing
                .metrics(Some(&empty), start + Duration::from_secs(10), false)
                .remaining,
            Some(Duration::ZERO)
        );
        assert_eq!(format_duration(Duration::from_secs(90_061)), "25:01:01");
    }

    #[test]
    fn pause_intervals_are_clipped_to_the_worker_completion_time() {
        let start = Instant::now();
        let mut timing = CompressionTiming::new(start);
        timing.set_paused(true, start + Duration::from_secs(4));
        timing.set_paused(false, start + Duration::from_secs(8));
        timing.finish(start + Duration::from_secs(6), true);

        let metrics = timing.metrics(
            Some(&progress(4 * MIB, 4 * MIB, 2 * MIB)),
            start + Duration::from_secs(10),
            false,
        );
        assert_eq!(metrics.elapsed, Duration::from_secs(6));
        assert_eq!(metrics.speed_mib_per_second, Some(1.0));
    }
}
