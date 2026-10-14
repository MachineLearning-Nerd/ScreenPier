use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct Allocation {
    pub(crate) kbps: u32,
    pub(crate) fps: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct AppliedAllocation {
    pub(crate) allocation: Allocation,
    pub(crate) committed_at: Instant,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum EstimateKind {
    Twcc { can_probe: bool },
    Remb,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct BandwidthObservation {
    pub(crate) estimate_bps: u64,
    pub(crate) observed_at: Instant,
    pub(crate) kind: EstimateKind,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum PendingChange {
    Reduction {
        allocation: Allocation,
        eligible_at: Instant,
    },
    Increase {
        minimum: Allocation,
        started_at: Instant,
    },
}

impl PendingChange {
    pub(crate) fn due_at(self) -> Instant {
        match self {
            Self::Reduction { eligible_at, .. } => eligible_at,
            Self::Increase { started_at, .. } => started_at + Duration::from_secs(3),
        }
    }
}

pub(crate) struct AllocationController {
    maximum_kbps: u32,
    applied: AppliedAllocation,
    latest: Option<BandwidthObservation>,
    pending: Option<PendingChange>,
    commits: u64,
}

impl AllocationController {
    pub(crate) fn new(maximum_kbps: u32, initial: Allocation, now: Instant) -> Self {
        assert!(maximum_kbps >= 200);
        assert!((200..=maximum_kbps).contains(&initial.kbps));
        Self {
            maximum_kbps,
            applied: AppliedAllocation {
                allocation: initial,
                committed_at: now,
            },
            latest: None,
            pending: None,
            commits: 0,
        }
    }

    pub(crate) fn observe(&mut self, observation: BandwidthObservation) {
        self.latest = Some(observation);
        let candidate = allocation_for(observation.estimate_bps, self.maximum_kbps);
        self.refresh_pending(candidate, observation.observed_at);
    }

    fn refresh_pending(&mut self, candidate: Allocation, now: Instant) {
        let current = u64::from(self.applied.allocation.kbps);
        let desired = u64::from(candidate.kbps);
        self.pending = if desired * 10 <= current * 9 {
            Some(PendingChange::Reduction {
                allocation: candidate,
                eligible_at: self.applied.committed_at + Duration::from_secs(1),
            })
        } else if desired * 10 >= current * 11 {
            let (minimum, started_at) = match self.pending {
                Some(PendingChange::Increase {
                    minimum,
                    started_at,
                }) => (
                    if candidate.kbps < minimum.kbps {
                        candidate
                    } else {
                        minimum
                    },
                    started_at,
                ),
                _ => (candidate, now),
            };
            Some(PendingChange::Increase {
                minimum,
                started_at,
            })
        } else {
            None
        };
    }

    pub(crate) fn take_due(&mut self, now: Instant) -> Option<Allocation> {
        let pending = self.pending?;
        if now < pending.due_at() {
            return None;
        }
        let allocation = match pending {
            PendingChange::Reduction { allocation, .. } => allocation,
            PendingChange::Increase { minimum, .. } => minimum,
        };
        self.applied = AppliedAllocation {
            allocation,
            committed_at: now,
        };
        self.pending = None;
        self.commits += 1;
        if let Some(latest) = self.latest {
            self.refresh_pending(allocation_for(latest.estimate_bps, self.maximum_kbps), now);
        }
        Some(allocation)
    }

    pub(crate) fn applied(&self) -> AppliedAllocation {
        self.applied
    }

    pub(crate) fn latest(&self) -> Option<BandwidthObservation> {
        self.latest
    }

    pub(crate) fn commits(&self) -> u64 {
        self.commits
    }

    pub(crate) fn candidate(&self) -> Option<Allocation> {
        self.latest
            .map(|latest| allocation_for(latest.estimate_bps, self.maximum_kbps))
    }

    pub(crate) fn pending(&self) -> Option<PendingChange> {
        self.pending
    }
}

pub(crate) fn allocation_for(estimate_bps: u64, maximum_kbps: u32) -> Allocation {
    assert!(maximum_kbps >= 200);
    let desired = estimate_bps / 10_000 * 8 + estimate_bps % 10_000 * 8 / 10_000;
    let kbps = desired.clamp(200, u64::from(maximum_kbps)) as u32;
    Allocation {
        kbps,
        fps: fps_for(kbps),
    }
}

pub(crate) fn fps_for(kbps: u32) -> u32 {
    if kbps >= 2500 {
        30
    } else if kbps >= 1500 {
        24
    } else if kbps >= 750 {
        15
    } else {
        8
    }
}

#[cfg(test)]
mod tests {
    use super::{
        Allocation, AllocationController, BandwidthObservation, EstimateKind, PendingChange,
        allocation_for,
    };
    use std::time::{Duration, Instant};

    fn at(start: Instant, millis: u64) -> Instant {
        start + Duration::from_millis(millis)
    }

    fn policy(start: Instant, kbps: u32) -> AllocationController {
        AllocationController::new(3000, Allocation { kbps, fps: 30 }, start)
    }

    fn observe(policy: &mut AllocationController, bps: u64, now: Instant) {
        policy.observe(BandwidthObservation {
            estimate_bps: bps,
            observed_at: now,
            kind: EstimateKind::Twcc { can_probe: true },
        });
    }

    #[test]
    fn stable_estimate_survives_initial_cooldown_without_another_event() {
        let start = Instant::now();
        let mut policy = policy(start, 3000);
        observe(&mut policy, 1_000_000, at(start, 200));
        assert_eq!(policy.take_due(at(start, 999)), None);
        assert_eq!(
            policy.applied().allocation,
            Allocation {
                kbps: 3000,
                fps: 30
            }
        );
        assert_eq!(
            policy.take_due(at(start, 1000)),
            Some(Allocation { kbps: 800, fps: 15 })
        );
        assert_eq!(policy.applied().committed_at, at(start, 1000));
        assert_eq!(policy.take_due(at(start, 5000)), None);
        assert_eq!(
            policy.applied().allocation,
            Allocation { kbps: 800, fps: 15 }
        );
    }

    #[test]
    fn ordinary_growth_reaches_840_after_three_seconds_without_another_event() {
        let start = Instant::now();
        let mut policy = policy(start, 700);
        observe(&mut policy, 1_050_000, at(start, 6000));
        assert_eq!(policy.take_due(at(start, 8999)), None);
        assert_eq!(
            policy.applied().allocation,
            Allocation { kbps: 700, fps: 30 }
        );
        assert_eq!(
            policy.take_due(at(start, 9000)),
            Some(Allocation { kbps: 840, fps: 15 })
        );
    }

    #[test]
    fn transport_headroom_reaches_2400() {
        let start = Instant::now();
        let mut policy = policy(start, 3000);
        observe(&mut policy, 3_000_000, at(start, 6000));
        assert_eq!(
            policy.take_due(at(start, 6000)),
            Some(Allocation {
                kbps: 2400,
                fps: 24
            })
        );
    }

    #[test]
    fn repeated_reductions_replace_target_without_renewing_deadline() {
        let start = Instant::now();
        let mut policy = policy(start, 3000);
        observe(&mut policy, 1_000_000, at(start, 200));
        observe(&mut policy, 2_000_000, at(start, 999));
        assert_eq!(
            policy.pending(),
            Some(PendingChange::Reduction {
                allocation: Allocation {
                    kbps: 1600,
                    fps: 24
                },
                eligible_at: at(start, 1000),
            })
        );
        assert_eq!(
            policy.take_due(at(start, 1000)),
            Some(Allocation {
                kbps: 1600,
                fps: 24
            })
        );
    }

    #[test]
    fn repeated_increases_keep_start_and_minimum_target() {
        let start = Instant::now();
        let mut policy = policy(start, 700);
        observe(&mut policy, 1_125_000, at(start, 200));
        observe(&mut policy, 1_050_000, at(start, 1000));
        observe(&mut policy, 1_250_000, at(start, 3199));
        assert_eq!(
            policy.pending(),
            Some(PendingChange::Increase {
                minimum: Allocation { kbps: 840, fps: 15 },
                started_at: at(start, 200),
            })
        );
        assert_eq!(policy.take_due(at(start, 3199)), None);
        assert_eq!(
            policy.take_due(at(start, 3200)),
            Some(Allocation { kbps: 840, fps: 15 })
        );
    }

    #[test]
    fn retained_higher_estimate_can_complete_a_second_increase_window() {
        let start = Instant::now();
        let mut policy = policy(start, 700);
        observe(&mut policy, 1_125_000, at(start, 200));
        observe(&mut policy, 1_500_000, at(start, 1000));
        assert_eq!(
            policy.take_due(at(start, 3200)),
            Some(Allocation { kbps: 900, fps: 15 })
        );
        assert_eq!(
            policy.pending(),
            Some(PendingChange::Increase {
                minimum: Allocation {
                    kbps: 1200,
                    fps: 15
                },
                started_at: at(start, 3200),
            })
        );
        assert_eq!(policy.take_due(at(start, 6199)), None);
        assert_eq!(
            policy.take_due(at(start, 6200)),
            Some(Allocation {
                kbps: 1200,
                fps: 15
            })
        );
        assert_eq!(policy.latest().unwrap().observed_at, at(start, 1000));
    }

    #[test]
    fn in_band_observation_cancels_reduction() {
        let start = Instant::now();
        let mut policy = policy(start, 3000);
        observe(&mut policy, 1_000_000, at(start, 200));
        observe(&mut policy, 3_750_000, at(start, 500));
        assert_eq!(policy.take_due(at(start, 1000)), None);
        assert_eq!(
            policy.applied().allocation,
            Allocation {
                kbps: 3000,
                fps: 30
            }
        );
        observe(&mut policy, 2_000_000, at(start, 1001));
        assert_eq!(
            policy.take_due(at(start, 1001)),
            Some(Allocation {
                kbps: 1600,
                fps: 24
            })
        );
    }

    #[test]
    fn in_band_observation_restarts_increase_confirmation() {
        let start = Instant::now();
        let mut policy = policy(start, 700);
        observe(&mut policy, 1_050_000, at(start, 200));
        observe(&mut policy, 875_000, at(start, 2000));
        observe(&mut policy, 1_050_000, at(start, 2500));
        assert_eq!(policy.take_due(at(start, 3200)), None);
        assert_eq!(
            policy.applied().allocation,
            Allocation { kbps: 700, fps: 30 }
        );
        assert_eq!(
            policy.take_due(at(start, 5500)),
            Some(Allocation { kbps: 840, fps: 15 })
        );
    }

    #[test]
    fn reduction_to_increase_starts_a_new_confirmation_window() {
        let start = Instant::now();
        let mut policy = policy(start, 1000);
        observe(&mut policy, 1_000_000, at(start, 200));
        observe(&mut policy, 1_500_000, at(start, 500));
        assert_eq!(policy.take_due(at(start, 1000)), None);
        assert_eq!(
            policy.applied().allocation,
            Allocation {
                kbps: 1000,
                fps: 30
            }
        );
        assert_eq!(policy.take_due(at(start, 3499)), None);
        assert_eq!(
            policy.take_due(at(start, 3500)),
            Some(Allocation {
                kbps: 1200,
                fps: 15
            })
        );
    }

    #[test]
    fn increase_to_reduction_uses_last_commit_time() {
        let start = Instant::now();
        let mut policy = policy(start, 1000);
        observe(&mut policy, 1_500_000, at(start, 200));
        observe(&mut policy, 1_000_000, at(start, 2500));
        assert_eq!(
            policy.take_due(at(start, 2500)),
            Some(Allocation { kbps: 800, fps: 15 })
        );
    }

    #[test]
    fn commit_starts_next_reduction_cooldown() {
        let start = Instant::now();
        let mut policy = policy(start, 3000);
        observe(&mut policy, 2_000_000, at(start, 5000));
        assert_eq!(
            policy.take_due(at(start, 5000)),
            Some(Allocation {
                kbps: 1600,
                fps: 24
            })
        );
        observe(&mut policy, 1_000_000, at(start, 5200));
        assert_eq!(policy.take_due(at(start, 5999)), None);
        assert_eq!(
            policy.applied().allocation,
            Allocation {
                kbps: 1600,
                fps: 24
            }
        );
        assert_eq!(
            policy.take_due(at(start, 6000)),
            Some(Allocation { kbps: 800, fps: 15 })
        );
    }

    #[test]
    fn inclusive_upper_deadband_accepts_1100() {
        let start = Instant::now();
        let mut policy = policy(start, 1000);
        observe(&mut policy, 1_375_000, at(start, 100));
        assert_eq!(
            policy.take_due(at(start, 3100)),
            Some(Allocation {
                kbps: 1100,
                fps: 15
            })
        );
    }

    #[test]
    fn upper_deadband_rejects_1099_then_accepts_1100() {
        let start = Instant::now();
        let mut policy = policy(start, 1000);
        observe(&mut policy, 1_373_750, at(start, 100));
        assert_eq!(policy.take_due(at(start, 5000)), None);
        assert_eq!(
            policy.applied().allocation,
            Allocation {
                kbps: 1000,
                fps: 30
            }
        );
        observe(&mut policy, 1_375_000, at(start, 5000));
        assert_eq!(
            policy.take_due(at(start, 8000)),
            Some(Allocation {
                kbps: 1100,
                fps: 15
            })
        );
    }

    #[test]
    fn inclusive_lower_deadband_accepts_900() {
        let start = Instant::now();
        let mut policy = policy(start, 1000);
        observe(&mut policy, 1_125_000, at(start, 100));
        assert_eq!(
            policy.take_due(at(start, 1000)),
            Some(Allocation { kbps: 900, fps: 15 })
        );
    }

    #[test]
    fn lower_deadband_rejects_901_then_accepts_900() {
        let start = Instant::now();
        let mut policy = policy(start, 1000);
        observe(&mut policy, 1_126_250, at(start, 100));
        assert_eq!(policy.take_due(at(start, 5000)), None);
        assert_eq!(
            policy.applied().allocation,
            Allocation {
                kbps: 1000,
                fps: 30
            }
        );
        observe(&mut policy, 1_125_000, at(start, 5000));
        assert_eq!(
            policy.take_due(at(start, 5000)),
            Some(Allocation { kbps: 900, fps: 15 })
        );
    }

    #[test]
    fn overflow_estimate_caps_before_narrowing() {
        assert_eq!(
            allocation_for(u64::MAX, 3000),
            Allocation {
                kbps: 3000,
                fps: 30
            }
        );
        assert_eq!(
            allocation_for(u64::MAX, u32::MAX),
            Allocation {
                kbps: u32::MAX,
                fps: 30
            }
        );
        assert_eq!(
            allocation_for(5_368_709_120_000, 3000),
            Allocation {
                kbps: 3000,
                fps: 30
            }
        );
    }

    #[test]
    fn maximum_and_existing_floor_remain_exact() {
        assert_eq!(
            allocation_for(100_000, 3000),
            Allocation { kbps: 200, fps: 8 }
        );
        assert_eq!(allocation_for(0, 3000), Allocation { kbps: 200, fps: 8 });
        assert_eq!(
            allocation_for(250_000, 3000),
            Allocation { kbps: 200, fps: 8 }
        );
        assert_eq!(
            allocation_for(5_000_000, 2500),
            Allocation {
                kbps: 2500,
                fps: 30
            }
        );
        assert_eq!(
            allocation_for(5_000_000, 200),
            Allocation { kbps: 200, fps: 8 }
        );
    }

    #[test]
    fn fps_tiers_have_exact_existing_boundaries() {
        assert_eq!(
            allocation_for(936_250, 3000),
            Allocation { kbps: 749, fps: 8 }
        );
        assert_eq!(
            allocation_for(937_500, 3000),
            Allocation { kbps: 750, fps: 15 }
        );
        assert_eq!(
            allocation_for(1_873_750, 3000),
            Allocation {
                kbps: 1499,
                fps: 15
            }
        );
        assert_eq!(
            allocation_for(1_875_000, 3000),
            Allocation {
                kbps: 1500,
                fps: 24
            }
        );
        assert_eq!(
            allocation_for(3_123_750, 3000),
            Allocation {
                kbps: 2499,
                fps: 24
            }
        );
        assert_eq!(
            allocation_for(3_125_000, 3000),
            Allocation {
                kbps: 2500,
                fps: 30
            }
        );
    }

    #[test]
    fn observation_age_is_retained_without_expiring_a_stable_estimate() {
        let start = Instant::now();
        let mut policy = policy(start, 700);
        observe(&mut policy, 1_050_000, at(start, 200));
        assert_eq!(
            policy.take_due(at(start, 60_000)),
            Some(Allocation { kbps: 840, fps: 15 })
        );
        assert_eq!(
            policy.latest(),
            Some(BandwidthObservation {
                estimate_bps: 1_050_000,
                observed_at: at(start, 200),
                kind: EstimateKind::Twcc { can_probe: true },
            })
        );
    }

    #[test]
    fn latest_observation_replaces_metadata_even_when_it_cancels_pending() {
        let start = Instant::now();
        let mut policy = policy(start, 1000);
        observe(&mut policy, 1_500_000, at(start, 200));
        policy.observe(BandwidthObservation {
            estimate_bps: 1_250_000,
            observed_at: at(start, 1000),
            kind: EstimateKind::Remb,
        });
        assert_eq!(
            policy.latest(),
            Some(BandwidthObservation {
                estimate_bps: 1_250_000,
                observed_at: at(start, 1000),
                kind: EstimateKind::Remb,
            })
        );
        assert_eq!(policy.take_due(at(start, 3200)), None);
        assert_eq!(
            policy.applied().allocation,
            Allocation {
                kbps: 1000,
                fps: 30
            }
        );
        observe(&mut policy, 1_500_000, at(start, 4000));
        assert_eq!(
            policy.take_due(at(start, 7000)),
            Some(Allocation {
                kbps: 1200,
                fps: 15
            })
        );
    }

    #[test]
    fn widened_deadband_works_at_u32_maximum() {
        let start = Instant::now();
        let mut policy = AllocationController::new(
            u32::MAX,
            Allocation {
                kbps: u32::MAX,
                fps: 30,
            },
            start,
        );
        observe(&mut policy, 4_500_000_000_000, at(start, 200));
        assert_eq!(
            policy.take_due(at(start, 1000)),
            Some(Allocation {
                kbps: 3_600_000_000,
                fps: 30
            })
        );
    }

    #[test]
    fn tick_without_an_observation_keeps_initial_configuration() {
        let start = Instant::now();
        let mut policy = policy(start, 700);
        assert_eq!(policy.take_due(at(start, 10_000)), None);
        assert_eq!(
            policy.applied().allocation,
            Allocation { kbps: 700, fps: 30 }
        );
        observe(&mut policy, 1_050_000, at(start, 10_000));
        assert_eq!(
            policy.take_due(at(start, 13_000)),
            Some(Allocation { kbps: 840, fps: 15 })
        );
    }
}
