use std::time::Duration;

use agent_launcher_core::{ActivityCompleteness, ActivitySample, HerdrActivitySnapshot};
use chrono::{DateTime, Utc};

const HISTORY_MS: i64 = 15 * 60 * 1000;
// The runtime's fixed sampling cadence. Empty slots remain gaps when downsampling.
const SAMPLE_MS: i64 = 2000;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct ActivityBucket {
    pub working: Option<u64>,
    pub partial: bool,
}

/// Retain chronology independently of counts and the runtime's bounded sample retention.
pub(crate) fn history_origin(
    snapshot: &HerdrActivitySnapshot,
    now: DateTime<Utc>,
    previous: Option<DateTime<Utc>>,
) -> Option<DateTime<Utc>> {
    snapshot
        .samples
        .iter()
        .map(|sample| sample.sampled_at)
        .chain(previous)
        .filter(|at| *at <= now)
        .min()
}

/// Whole cells strictly before the first observed two-dot bucket, including missing samples.
pub(crate) fn unobserved_columns(
    snapshot: &HerdrActivitySnapshot,
    width: usize,
    now: DateTime<Utc>,
    origin: Option<DateTime<Utc>>,
) -> usize {
    let Some(first) = history_origin(snapshot, now, origin) else {
        return width;
    };
    let age = now.signed_duration_since(first).num_milliseconds();
    if age >= HISTORY_MS {
        return 0;
    }
    let slot = ((HISTORY_MS - 1 - age) / SAMPLE_MS) as usize;
    (slot * (width * 2) / (HISTORY_MS / SAMPLE_MS) as usize) / 2
}

/// Project persisted UTC observations, never launcher events or current-state backfill.
pub(crate) fn buckets(
    snapshot: &HerdrActivitySnapshot,
    width: usize,
    now: DateTime<Utc>,
) -> Vec<ActivityBucket> {
    if width == 0 {
        return Vec::new();
    }
    let mut slots = vec![ActivityBucket::default(); (HISTORY_MS / SAMPLE_MS) as usize];
    for sample in &snapshot.samples {
        if sample.sampled_at > now {
            continue;
        }
        let age = now
            .signed_duration_since(sample.sampled_at)
            .num_milliseconds();
        if !(0..HISTORY_MS).contains(&age) {
            continue;
        }
        let slot = &mut slots[((HISTORY_MS - 1 - age) / SAMPLE_MS) as usize];
        if sample.completeness != ActivityCompleteness::Missing
            && let Some(counts) = &sample.counts
        {
            slot.working = Some(slot.working.unwrap_or(0).max(counts.working));
        }
        slot.partial |= sample.completeness != ActivityCompleteness::Complete
            || !sample.inventory_complete
            || sample.counts.is_none();
    }
    let mut result = vec![ActivityBucket::default(); width];
    for (index, slot) in slots.iter().enumerate() {
        let bucket = &mut result[index * width / slots.len()];
        if let Some(count) = slot.working {
            bucket.working = Some(bucket.working.unwrap_or(0).max(count));
        }
        bucket.partial |= slot.partial || slot.working.is_none();
    }
    result
}

pub(crate) fn current_sample(
    snapshot: &HerdrActivitySnapshot,
    now: DateTime<Utc>,
) -> Option<&ActivitySample> {
    snapshot
        .samples
        .iter()
        .filter(|sample| {
            let age = now.signed_duration_since(sample.sampled_at);
            age >= chrono::Duration::zero() && age <= chrono::Duration::seconds(10)
        })
        .max_by_key(|sample| sample.sampled_at)
}

const DEMO_COUNTS: [u64; 30] = [
    0, 0, 1, 3, 4, 2, 1, 0, 0, 2, 6, 4, 5, 8, 5, 3, 1, 0, 1, 4, 3, 4, 6, 4, 2, 0, 2, 7, 4, 3,
];
pub(crate) const DEMO_PRECISION: u64 = 1024;
const DEMO_SAMPLE_NS: u128 = 40_000_000;
const DEMO_WINDOW_NS: u128 = 450 * DEMO_SAMPLE_NS;

// Positions are nanoseconds times dot width, so even fractional dot spacing is exact.
// Reduce elapsed time before multiplying to keep Duration::MAX bounded.
fn demo_position(elapsed: Duration, dots: u128) -> (u128, u128) {
    let ns = elapsed.as_nanos();
    let phase = (ns % DEMO_WINDOW_NS) * dots;
    let unobserved = (DEMO_WINDOW_NS.saturating_sub(ns) * dots).div_ceil(DEMO_WINDOW_NS);
    (phase, unobserved)
}

/// Renderer-only fractional projection. Live observations never take this path.
/// One window traverses the available width in 18 seconds, independent of redraws.
pub(crate) fn demo_buckets(elapsed: Duration, dots: usize) -> Vec<ActivityBucket> {
    let (position, unobserved) = demo_position(elapsed, dots as u128);
    let period = DEMO_WINDOW_NS * dots as u128;
    let sample = DEMO_SAMPLE_NS * dots as u128;
    (0..dots)
        .map(|x| {
            if (x as u128) < unobserved {
                return ActivityBucket::default();
            }
            let phase = (position + x as u128 * DEMO_WINDOW_NS) % period;
            let missing = (150 * sample..180 * sample).contains(&phase);
            let partial = (300 * sample..325 * sample).contains(&phase);
            // A narrow display can skip an entire outage between two plotted dots.
            // Break the connection whenever their interval crosses a coverage boundary.
            let crosses_coverage = [150, 180, 300, 325]
                .iter()
                .any(|boundary| (phase + period - boundary * sample) % period < DEMO_WINDOW_NS);
            let knot = 5 * sample;
            let index = (phase / knot) as usize % DEMO_COUNTS.len();
            let fraction = phase % knot;
            let a = u128::from(DEMO_COUNTS[index]);
            let b = u128::from(DEMO_COUNTS[(index + 1) % DEMO_COUNTS.len()]);
            ActivityBucket {
                working: (!missing).then_some(
                    (((a * (knot - fraction) + b * fraction) * u128::from(DEMO_PRECISION)
                        + knot / 2)
                        / knot) as u64,
                ),
                partial: partial || missing || crosses_coverage,
            }
        })
        .collect()
}

pub(crate) fn demo_unobserved_columns(elapsed: Duration, width: usize) -> usize {
    (demo_position(elapsed, width as u128 * 2).1 / 2) as usize
}

/// Renderer-only fixture: integral counters, never sent to the collector or persisted.
pub(crate) fn demo_snapshot(elapsed: Duration, now: DateTime<Utc>) -> HerdrActivitySnapshot {
    let total = elapsed.as_nanos() / DEMO_SAMPLE_NS;
    let start = total.saturating_sub(450);
    HerdrActivitySnapshot {
        enabled: true,
        samples: (start..total)
            .map(|index| {
                let phase = (index % 450) as usize;
                let missing = (150..180).contains(&phase);
                let partial = (300..325).contains(&phase);
                ActivitySample {
                    sampled_at: now
                        - chrono::Duration::milliseconds((total - 1 - index) as i64 * SAMPLE_MS),
                    counts: (!missing).then_some(agent_launcher_core::ActivityCounts {
                        working: DEMO_COUNTS[phase / 5 % DEMO_COUNTS.len()],
                        blocked: u64::from(partial),
                        ..Default::default()
                    }),
                    expected_endpoints: 2,
                    fresh_endpoints: if missing {
                        0
                    } else if partial {
                        1
                    } else {
                        2
                    },
                    stale_endpoints: if missing {
                        2
                    } else {
                        usize::from(partial)
                    },
                    never_observed_endpoints: 0,
                    failed_endpoints: 0,
                    excluded_endpoints: 0,
                    inventory_complete: true,
                    completeness: if missing {
                        ActivityCompleteness::Missing
                    } else if partial {
                        ActivityCompleteness::Partial
                    } else {
                        ActivityCompleteness::Complete
                    },
                }
            })
            .collect(),
        ..Default::default()
    }
}

#[cfg(test)]
pub(crate) fn sample(
    at: DateTime<Utc>,
    working: u64,
    completeness: ActivityCompleteness,
) -> ActivitySample {
    ActivitySample {
        sampled_at: at,
        counts: (completeness != ActivityCompleteness::Missing).then_some(
            agent_launcher_core::ActivityCounts {
                working,
                ..Default::default()
            },
        ),
        expected_endpoints: 1,
        fresh_endpoints: usize::from(completeness == ActivityCompleteness::Complete),
        stale_endpoints: 0,
        never_observed_endpoints: 0,
        failed_endpoints: 0,
        excluded_endpoints: 0,
        inventory_complete: completeness == ActivityCompleteness::Complete,
        completeness,
    }
}

#[cfg(test)]
mod tests {
    use ActivityCompleteness::{Complete, Missing, Partial};

    use super::*;

    #[test]
    fn demo_reveal_is_monotonic_and_moves_at_most_one_dot_per_frame() {
        for width in 0..=270 {
            let dots = width * 2;
            let mut previous = dots;
            for frame in 0..=180 {
                let elapsed = Duration::from_nanos(frame * (1_000_000_000 / 30));
                let data = demo_buckets(elapsed, dots);
                let first = data
                    .iter()
                    .position(|b| b.working.is_some())
                    .unwrap_or(dots);
                assert!(first <= previous, "width={width}, frame={frame}");
                assert!(previous - first <= 1, "width={width}, frame={frame}");
                assert_eq!(demo_unobserved_columns(elapsed, width), first / 2);
                previous = first;
            }
            for elapsed in [18, 19, 36, 3600, u64::from(u32::MAX)]
                .map(Duration::from_secs)
                .into_iter()
                .chain([Duration::MAX])
            {
                assert_eq!(demo_unobserved_columns(elapsed, width), 0);
                let data = demo_buckets(elapsed, dots);
                assert_eq!(data.len(), dots);
                assert!(
                    data.iter()
                        .filter_map(|b| b.working)
                        .all(|v| v <= 8 * DEMO_PRECISION)
                );
                // Rendering the same elapsed time never advances/restarts the fixture.
                assert_eq!(data, demo_buckets(elapsed, dots));
            }
        }
        assert!(demo_buckets(Duration::MAX, 0).is_empty());
        assert_eq!(
            demo_unobserved_columns(Duration::ZERO, usize::MAX),
            usize::MAX
        );
    }

    #[test]
    fn demo_fractional_projection_scrolls_without_quantizing_to_sample_slots() {
        let elapsed = Duration::from_secs(18);
        let data = demo_buckets(elapsed, 300);
        let next = demo_buckets(elapsed + Duration::from_secs_f64(1.0 / 30.0), 300);
        assert_ne!(data, next);
        assert!(
            next.iter()
                .filter_map(|b| b.working)
                .any(|v| v % DEMO_PRECISION != 0)
        );
        // Moving one dot's worth of time is exactly a one-dot translation, not a batch.
        let shifted = demo_buckets(elapsed + Duration::from_millis(60), 300);
        assert_eq!(data[1..], shifted[..299]);
        for (x, bucket) in data.iter().enumerate() {
            let phase = x as f64 * 1.5;
            if (150.0..180.0).contains(&phase) {
                assert_eq!(bucket.working, None);
            }
            if (300.0..325.0).contains(&phase) {
                assert!(bucket.partial);
            }
        }
        // At this width no dot lands inside the gap, but no line may bridge it.
        let narrow = demo_buckets(elapsed, 3);
        assert!(narrow[2].partial);
    }

    #[test]
    fn demo_translation_and_coverage_boundaries_are_exact() {
        for dots in [0, 1, 2, 3, 4, 48, 100, 128, 300, 450, 1000] {
            if dots == 0 {
                assert!(demo_buckets(Duration::MAX, dots).is_empty());
                continue;
            }
            let step = Duration::from_nanos((DEMO_WINDOW_NS / dots as u128) as u64);
            for ns in [
                0,
                1,
                33_333_333,
                5_999_999_999,
                6_000_000_000,
                12_999_999_999,
            ] {
                let elapsed = Duration::from_secs(18) + Duration::from_nanos(ns);
                let data = demo_buckets(elapsed, dots);
                let shifted = demo_buckets(elapsed + step, dots);
                assert_eq!(data[1..], shifted[..dots - 1], "dots={dots}, ns={ns}");
                assert_eq!(data, demo_buckets(elapsed + Duration::from_secs(18), dots));
            }
        }
        let at = Duration::from_secs(18);
        let before = demo_buckets(at - Duration::from_nanos(1), 450);
        let exact = demo_buckets(at, 450);
        let after = demo_buckets(at + Duration::from_nanos(1), 450);
        assert!(before[150].working.is_some());
        assert!(exact[150].working.is_none());
        assert!(after[150].working.is_none());
        assert!(before[180].working.is_none());
        assert!(exact[180].working.is_some() && exact[180].partial);
        assert!(!before[300].partial);
        assert!(exact[300].partial && after[300].partial);
        assert!(exact[325].partial); // The incoming interval crosses the partial edge.
        assert!(!exact[326].partial);

        let reduced = Duration::from_nanos(
            (DEMO_WINDOW_NS + Duration::MAX.as_nanos() % DEMO_WINDOW_NS) as u64,
        );
        assert_eq!(demo_buckets(Duration::MAX, 540), demo_buckets(reduced, 540));
        let now = DateTime::from_timestamp(1_800_000_000, 0).unwrap();
        let huge = demo_snapshot(Duration::MAX, now);
        assert_eq!(huge.samples, demo_snapshot(reduced, now).samples);
    }

    #[test]
    fn leading_columns_follow_sample_slots_not_counts_or_input_order() {
        let now = DateTime::from_timestamp(1_800_000_000, 0).unwrap();
        for width in [0, 1, 24, 61, 62, 104, 225, 450, 1000] {
            let mut snapshot = HerdrActivitySnapshot::default();
            assert_eq!(unobserved_columns(&snapshot, width, now, None), width);
            snapshot
                .samples
                .push(sample(now + chrono::Duration::nanoseconds(1), 99, Complete));
            assert_eq!(unobserved_columns(&snapshot, width, now, None), width);
            assert!(
                buckets(&snapshot, width * 2, now)
                    .iter()
                    .all(|b| b.working.is_none())
            );
            for age in [0, 1999, 2000, 450_000, 899_999, 900_000, 960_000] {
                for completeness in [Complete, Partial, Missing] {
                    snapshot.samples.truncate(1);
                    snapshot.samples.push(sample(now, 0, Complete));
                    snapshot.samples.push(sample(
                        now - chrono::Duration::milliseconds(age),
                        0,
                        completeness,
                    ));
                    let expected = if age >= HISTORY_MS {
                        0
                    } else {
                        ((HISTORY_MS - 1 - age) / SAMPLE_MS) as usize * (width * 2) / 450 / 2
                    };
                    assert_eq!(unobserved_columns(&snapshot, width, now, None), expected);
                    let data = buckets(&snapshot, width * 2, now);
                    assert!(data[..expected * 2].iter().all(|b| b.working.is_none()));
                }
            }
        }
    }

    #[test]
    fn retained_origin_prevents_reveal_after_pruning_and_empty_snapshots() {
        let now = DateTime::from_timestamp(1_800_000_000, 0).unwrap();
        let mut snapshot = HerdrActivitySnapshot {
            samples: vec![sample(now - chrono::Duration::minutes(16), 0, Missing)],
            ..Default::default()
        };
        let origin = history_origin(&snapshot, now, None);
        snapshot.samples = vec![sample(now, 0, Missing)];
        assert_eq!(history_origin(&snapshot, now, origin), origin);
        snapshot.samples.clear();
        assert_eq!(history_origin(&snapshot, now, origin), origin);
        for width in [24, 104, 450, 1000] {
            assert_eq!(unobserved_columns(&snapshot, width, now, origin), 0);
            assert_eq!(
                unobserved_columns(&demo_snapshot(Duration::ZERO, now), width, now, None),
                width
            );
            assert!(
                unobserved_columns(
                    &demo_snapshot(Duration::from_millis(80), now),
                    width,
                    now,
                    None
                ) < width
            );
            let partial = unobserved_columns(
                &demo_snapshot(Duration::from_millis(8960), now),
                width,
                now,
                None,
            );
            assert!(partial > 0 && partial < width);
            for tick in [225, 226, 450, 10_000, u32::MAX] {
                assert_eq!(
                    unobserved_columns(
                        &demo_snapshot(Duration::from_millis(u64::from(tick) * 80), now),
                        width,
                        now,
                        None
                    ),
                    0
                );
            }
        }
    }

    #[test]
    fn counts_are_not_scores_or_capped() {
        let now = DateTime::from_timestamp(1_800_000_000, 0).unwrap();
        for count in [0, 1, 3, 1000, u64::MAX] {
            let snapshot = HerdrActivitySnapshot {
                samples: vec![sample(now, count, Complete)],
                ..Default::default()
            };
            let values = buckets(&snapshot, 450, now);
            assert_eq!(values[449], ActivityBucket {
                working: Some(count),
                partial: false
            });
            assert_eq!(values.iter().filter(|v| v.working.is_some()).count(), 1);
        }
    }

    #[test]
    fn persisted_times_leave_downtime_gaps_and_ignore_future_and_expired_rows() {
        let now = DateTime::from_timestamp(1_800_000_000, 0).unwrap();
        let snapshot = HerdrActivitySnapshot {
            samples: vec![
                sample(now - chrono::Duration::seconds(600), 3, Complete),
                sample(now, 0, Complete),
                sample(now + chrono::Duration::seconds(2), 99, Complete),
                sample(now - chrono::Duration::minutes(16), 99, Complete),
            ],
            ..Default::default()
        };
        let values = buckets(&snapshot, 450, now);
        assert_eq!(values[149].working, Some(3));
        assert_eq!(values[449].working, Some(0));
        assert!(values[150..449].iter().all(|v| v.working.is_none()));
        assert_eq!(current_sample(&snapshot, now).unwrap().sampled_at, now);
        assert!(current_sample(&snapshot, now + chrono::Duration::seconds(20)).is_none());
    }

    #[test]
    fn complete_empty_history_is_zero_and_downsampling_preserves_peaks() {
        let now = DateTime::from_timestamp(1_800_000_000, 0).unwrap();
        let mut snapshot = HerdrActivitySnapshot {
            samples: (0..450)
                .map(|index| sample(now - chrono::Duration::seconds(index * 2), 0, Complete))
                .collect(),
            ..Default::default()
        };
        for width in [1, 24, 44, 108] {
            assert!(buckets(&snapshot, width, now).iter().all(|bucket| *bucket
                == ActivityBucket {
                    working: Some(0),
                    partial: false
                }));
        }
        snapshot.samples[0].counts.as_mut().unwrap().working = 3;
        snapshot.samples[1].counts.as_mut().unwrap().working = 1000;
        assert_eq!(buckets(&snapshot, 1, now), [ActivityBucket {
            working: Some(1000),
            partial: false
        }]);
    }

    #[test]
    fn missing_partial_zero_and_downsampled_gaps_remain_distinct() {
        let now = DateTime::from_timestamp(1_800_000_000, 0).unwrap();
        let snapshot = HerdrActivitySnapshot {
            samples: vec![
                sample(now, 0, Complete),
                sample(now - chrono::Duration::seconds(2), 0, Partial),
                sample(now - chrono::Duration::seconds(4), 0, Missing),
            ],
            ..Default::default()
        };
        let values = buckets(&snapshot, 450, now);
        assert_eq!(values[449], ActivityBucket {
            working: Some(0),
            partial: false
        });
        assert_eq!(values[448], ActivityBucket {
            working: Some(0),
            partial: true
        });
        assert_eq!(values[447].working, None);
        assert_eq!(buckets(&snapshot, 1, now), [ActivityBucket {
            working: Some(0),
            partial: true
        }]);
        assert!(buckets(&snapshot, 0, now).is_empty());
    }

    #[test]
    fn braille_bucket_widths_preserve_hidden_gaps_and_partial_peaks() {
        let now = DateTime::from_timestamp(1_800_000_000, 0).unwrap();
        let mut snapshot = HerdrActivitySnapshot {
            samples: (0..450)
                .map(|index| sample(now - chrono::Duration::seconds(index * 2), 0, Complete))
                .collect(),
            ..Default::default()
        };
        snapshot.samples[1] = sample(now - chrono::Duration::seconds(2), u64::MAX, Partial);
        snapshot.samples[2] = sample(now - chrono::Duration::seconds(4), 0, Missing);
        for columns in [1, 2, 24, 44, 108, 225, 450] {
            let values = buckets(&snapshot, columns * 2, now);
            assert_eq!(values.len(), columns * 2);
            let peak = values.iter().find(|b| b.working == Some(u64::MAX)).unwrap();
            assert!(peak.partial);
            let missing_slot_bucket = 447 * columns * 2 / 450;
            assert!(values[missing_slot_bucket].partial);
            if columns >= 225 {
                assert!(values[missing_slot_bucket].working.is_none());
            }
            if columns > 225 {
                // Upsampling must not invent observations between the fixed two-second slots.
                assert!(values[1].working.is_none());
            }
        }
    }
}
