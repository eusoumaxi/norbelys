//! Which variant of a step a person receives, and when a test of variants has a winner.
//!
//! # Allocation
//!
//! A published step revision offers one or more variants, each with a weight from 1 to 100. A
//! person enrolled in the campaign receives exactly one of them for that revision, chosen once
//! and kept (`step_assignments`), by the revision's allocation:
//!
//! - **`balanced`**: every variant receives the same share; the next person gets the variant
//!   assigned least so far.
//! - **`weighted`**: shares follow the weights; the next person gets the variant furthest below
//!   its share, which is the variant whose count, plus this person, divided by its weight is
//!   smallest. Three to one weights give exactly three to one assignments, in an interleaved
//!   order, without a random source.
//! - **`automatic`**: balanced while the test runs, then the winner for everyone (below).
//!
//! The choice is deterministic: it reads the counts of earlier assignments, and ties go to the
//! variant listed first. Determinism makes the rule provable by the tests below and makes a run
//! that is repeated (a job retried after a crash) choose what the first run chose for the same
//! counts. A winner, whoever chose it, overrides every allocation: it is the variant a step
//! revision now sends to everyone.
//!
//! # Winner selection
//!
//! An `automatic` revision names a winner when its rule is met:
//!
//! 1. the revision has offered at least two variants (one variant has nothing to compare);
//! 2. the observation window has passed since the revision was published, so slow signals
//!    (replies arrive over days) are counted before anyone wins;
//! 3. every variant was sent to at least the minimum sample, so a variant is never declared
//!    better on a handful of messages.
//!
//! The winner is the variant with the highest rate of the objective (opens, clicks or replies
//! per message sent); a tie goes to the variant listed first. Rates are compared by
//! cross-multiplication of whole counts, so no floating point decides a winner.

use std::time::Duration;

/// How a step revision assigns its variants (`step_revisions.allocation`).
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    strum::EnumIter,
    strum::IntoStaticStr,
    strum::EnumString,
    serde::Serialize,
    serde::Deserialize,
    utoipa::ToSchema,
)]
#[strum(serialize_all = "snake_case")]
#[serde(rename_all = "snake_case")]
pub enum Allocation {
    /// Every variant gets the same share.
    Balanced,
    /// Shares follow the variants' weights.
    Weighted,
    /// Balanced until the winner rule names a winner, then the winner for everyone.
    Automatic,
}

impl Allocation {
    /// The allocation as stored.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        self.into()
    }
}

/// What a test of variants is decided by (`step_revisions.ranking_objective`).
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    strum::EnumIter,
    strum::IntoStaticStr,
    strum::EnumString,
    serde::Serialize,
    serde::Deserialize,
    utoipa::ToSchema,
)]
#[strum(serialize_all = "snake_case")]
#[serde(rename_all = "snake_case")]
pub enum Objective {
    /// Unique messages opened by a person.
    Opens,
    /// Unique messages with a click.
    Clicks,
    /// Messages answered.
    Replies,
}

impl Objective {
    /// The objective as stored.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        self.into()
    }
}

/// When an `automatic` revision names its winner.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WinnerRule {
    /// What the variants are ranked by.
    pub objective: Objective,
    /// How long after the revision was published the results are read.
    pub observation_window: Duration,
    /// The messages every variant must have sent first.
    pub minimum_sample: u64,
}

/// One variant offered by a revision, as the allocation sees it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Offered {
    /// Its weight, 1 to 100; a weight of 0 is read as 1, so no variant is starved by a bad row.
    pub weight: u32,
    /// The people it has been assigned so far in this revision.
    pub assigned: u64,
}

/// The variant (an index into `offered`) the next person receives; `None` only when nothing is
/// offered. A `winner` (an index) overrides the allocation.
#[must_use]
pub fn choose(allocation: Allocation, offered: &[Offered], winner: Option<usize>) -> Option<usize> {
    if let Some(winner) = winner.filter(|index| *index < offered.len()) {
        return Some(winner);
    }
    let weighted = allocation == Allocation::Weighted;
    let share = |option: &Offered| -> (u128, u128) {
        let weight = if weighted { option.weight.max(1) } else { 1 };
        (
            u128::from(option.assigned).saturating_add(1),
            u128::from(weight),
        )
    };
    let mut best: Option<(usize, (u128, u128))> = None;
    for (index, option) in offered.iter().enumerate() {
        let (count, weight) = share(option);
        best = match best {
            // `count / weight < best_count / best_weight`, by cross-multiplication; a tie keeps
            // the earlier variant.
            Some((_, (best_count, best_weight)))
                if count.saturating_mul(best_weight) >= best_count.saturating_mul(weight) =>
            {
                best
            }
            _ => Some((index, (count, weight))),
        };
    }
    best.map(|(index, _)| index)
}

/// What one variant of a revision achieved, from the campaign's counters.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Observed {
    /// Messages sent.
    pub sent: u64,
    /// Unique messages opened by a person.
    pub opened: u64,
    /// Unique messages with a click.
    pub clicked: u64,
    /// Messages answered.
    pub replied: u64,
}

impl Observed {
    /// The count the objective ranks by.
    #[must_use]
    pub fn of(&self, objective: Objective) -> u64 {
        match objective {
            Objective::Opens => self.opened,
            Objective::Clicks => self.clicked,
            Objective::Replies => self.replied,
        }
    }
}

/// The winner (an index into `observed`) once `rule` is met, the revision having been published
/// `elapsed` ago; `None` while the test must go on (see the module).
#[must_use]
pub fn select_winner(rule: &WinnerRule, elapsed: Duration, observed: &[Observed]) -> Option<usize> {
    if observed.len() < 2 || elapsed < rule.observation_window {
        return None;
    }
    if observed
        .iter()
        .any(|variant| variant.sent < rule.minimum_sample.max(1))
    {
        return None;
    }
    let mut best: Option<(usize, u128, u128)> = None;
    for (index, variant) in observed.iter().enumerate() {
        let hits = u128::from(variant.of(rule.objective));
        let sent = u128::from(variant.sent);
        best = match best {
            // `hits / sent > best_hits / best_sent`; a tie keeps the earlier variant.
            Some((_, best_hits, best_sent))
                if hits.saturating_mul(best_sent) <= best_hits.saturating_mul(sent) =>
            {
                best
            }
            _ => Some((index, hits, sent)),
        };
    }
    best.map(|(index, ..)| index)
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use strum::IntoEnumIterator as _;

    use super::{Allocation, Objective, Observed, Offered, WinnerRule, choose, select_winner};

    /// Runs `people` assignments through `choose`, as the creators do (each choice counted
    /// before the next), and returns how many each variant received.
    fn assign(allocation: Allocation, weights: &[u32], people: u64) -> Vec<u64> {
        let mut offered: Vec<Offered> = weights
            .iter()
            .map(|weight| Offered {
                weight: *weight,
                assigned: 0,
            })
            .collect();
        for _ in 0..people {
            let index = choose(allocation, &offered, None).expect("a variant is offered");
            offered[index].assigned += 1;
        }
        offered.iter().map(|option| option.assigned).collect()
    }

    /// Every allocation splits people as its rule says: balanced and automatic (before a
    /// winner) equally whatever the weights, weighted in proportion to the weights. Generated
    /// from the enum, so a new allocation fails here until it has an expected split.
    #[test]
    fn every_allocation_splits_people_by_its_rule() {
        for allocation in Allocation::iter() {
            let split = assign(allocation, &[3, 1], 400);
            let expected = match allocation {
                Allocation::Balanced | Allocation::Automatic => vec![200, 200],
                Allocation::Weighted => vec![300, 100],
            };
            assert_eq!(split, expected, "{allocation:?}");
            assert_eq!(
                allocation.as_str().parse::<Allocation>().ok(),
                Some(allocation)
            );
        }
    }

    /// Over fifty variants, the most a step offers, every allocation still gives each person
    /// exactly one variant (the counts sum to the people), reaches every variant, and splits as
    /// its rule says: equally, or exactly in proportion to weights 1 to 50. Generated from the
    /// enum, so a new allocation fails here until it has an expected split at that size too.
    #[test]
    fn every_allocation_reaches_all_of_fifty_variants() {
        let weights: Vec<u32> = (1..=50).collect();
        let total: u64 = weights.iter().copied().map(u64::from).sum();
        for allocation in Allocation::iter() {
            let people = 2 * total;
            let split = assign(allocation, &weights, people);
            assert_eq!(split.iter().sum::<u64>(), people, "{allocation:?}");
            assert!(split.iter().all(|count| *count > 0), "{allocation:?}");
            let expected: Vec<u64> = match allocation {
                Allocation::Balanced | Allocation::Automatic => vec![people / 50; 50],
                Allocation::Weighted => weights
                    .iter()
                    .map(|weight| 2 * u64::from(*weight))
                    .collect(),
            };
            assert_eq!(split, expected, "{allocation:?}");
        }
    }

    /// A weighted split interleaves instead of sending a run of one variant first, so a test
    /// stopped early still saw every variant; ties go to the variant listed first; a weight of
    /// zero is read as one rather than starving its variant.
    #[test]
    fn a_weighted_split_interleaves_and_breaks_ties_in_order() {
        let mut offered = [
            Offered {
                weight: 1,
                assigned: 0,
            },
            Offered {
                weight: 1,
                assigned: 0,
            },
        ];
        let mut order = Vec::new();
        for _ in 0..4 {
            let index = choose(Allocation::Weighted, &offered, None).expect("offered");
            offered[index].assigned += 1;
            order.push(index);
        }
        assert_eq!(order, [0, 1, 0, 1]);
        assert_eq!(assign(Allocation::Weighted, &[0, 1], 10), vec![5, 5]);
        assert_eq!(assign(Allocation::Weighted, &[2, 1, 1], 8), vec![4, 2, 2]);
    }

    /// A winner overrides every allocation; a winner index that is not offered is ignored; an
    /// empty offer chooses nothing.
    #[test]
    fn a_winner_overrides_every_allocation() {
        let offered = [
            Offered {
                weight: 1,
                assigned: 0,
            },
            Offered {
                weight: 1,
                assigned: 5,
            },
        ];
        for allocation in Allocation::iter() {
            assert_eq!(choose(allocation, &offered, Some(1)), Some(1));
            assert_eq!(choose(allocation, &offered, Some(7)), Some(0));
            assert_eq!(choose(allocation, &[], None), None);
        }
    }

    fn rule(objective: Objective) -> WinnerRule {
        WinnerRule {
            objective,
            observation_window: Duration::from_secs(3_600),
            minimum_sample: 100,
        }
    }

    /// Each objective ranks by its own count: the variant with the higher rate of opens, clicks
    /// or replies wins, rates compared per message sent (a variant that sent more does not win
    /// by volume).
    #[test]
    fn each_objective_ranks_by_its_own_rate() {
        for objective in Objective::iter() {
            let strong = |count: u64| match objective {
                Objective::Opens => Observed {
                    sent: 200,
                    opened: count,
                    ..Observed::default()
                },
                Objective::Clicks => Observed {
                    sent: 200,
                    clicked: count,
                    ..Observed::default()
                },
                Objective::Replies => Observed {
                    sent: 200,
                    replied: count,
                    ..Observed::default()
                },
            };
            let weak = Observed {
                sent: 1_000,
                opened: 100,
                clicked: 100,
                replied: 100,
            };
            // 30 of 200 (15 %) beats 100 of 1,000 (10 %).
            assert_eq!(
                select_winner(
                    &rule(objective),
                    Duration::from_secs(3_600),
                    &[weak, strong(30)]
                ),
                Some(1),
                "{objective:?}"
            );
            assert_eq!(
                objective.as_str().parse::<Objective>().ok(),
                Some(objective)
            );
        }
    }

    /// Among fifty variants, the most a step offers, the winner is the one with the best rate
    /// wherever it is listed, and a tie between the best goes to the first of them listed, so a
    /// test of many variants is decided by the whole list.
    #[test]
    fn the_best_of_fifty_variants_wins() {
        let rule = rule(Objective::Replies);
        let window = Duration::from_secs(3_600);
        let mut observed = vec![
            Observed {
                sent: 100,
                replied: 5,
                ..Observed::default()
            };
            50
        ];
        observed[37].replied = 9;
        assert_eq!(select_winner(&rule, window, &observed), Some(37));
        observed[44].replied = 9;
        assert_eq!(select_winner(&rule, window, &observed), Some(37));
        observed[49].replied = 10;
        assert_eq!(select_winner(&rule, window, &observed), Some(49));
    }

    /// No winner before the rule is met: a single variant, a window not yet passed, a variant
    /// below the minimum sample. A tie goes to the variant listed first.
    #[test]
    fn no_winner_before_the_rule_is_met() {
        let rule = rule(Objective::Replies);
        let enough = Observed {
            sent: 100,
            replied: 10,
            ..Observed::default()
        };
        let short = Observed {
            sent: 99,
            replied: 50,
            ..Observed::default()
        };
        let window = Duration::from_secs(3_600);
        assert_eq!(select_winner(&rule, window, &[enough]), None);
        assert_eq!(
            select_winner(&rule, window - Duration::from_secs(1), &[enough, enough]),
            None
        );
        assert_eq!(select_winner(&rule, window, &[enough, short]), None);
        assert_eq!(select_winner(&rule, window, &[enough, enough]), Some(0));
    }
}
