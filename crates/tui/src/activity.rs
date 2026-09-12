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
