use std::collections::BTreeMap;

use super::{Candidate, Stays};

pub(super) type Holdings = BTreeMap<(String, String, String), usize>;

pub(super) fn spreads_sessions(mode: Stays, routing: &str) -> bool {
    matches!(mode, Stays::Session | Stays::Strict)
        && matches!(routing, "" | "smart" | "usage" | "pace")
}

pub(super) fn keeps_week_low_routing(routing: &str) -> bool {
    matches!(routing, "" | "smart" | "pace")
}

pub(super) fn back_to_main(candidates: &[Candidate], at: usize) -> bool {
    at > 0 && candidates[at].fallback && !candidates[0].fallback && !candidates[0].resting
}

pub(super) fn back_to_first(candidates: &[Candidate], at: usize) -> bool {
    at > 0
        && candidates[at].place > 0
        && !candidates[0].resting
        && !candidates[0].aside
        && candidates[0].group_member != candidates[at].group_member
}

pub(super) fn back_from_stand_in(candidates: &[Candidate], at: usize) -> bool {
    at > 0
        && !candidates[0].resting
        && !candidates[0].aside
        && candidates[0].place > 0
        && candidates[0].place < candidates[at].place
}

pub(super) fn stands_in_for(
    answerer: &Candidate,
    planned: &[Candidate],
    mut resting: impl FnMut(&Candidate) -> bool,
) -> bool {
    if answerer.place <= 1 {
        return false;
    }
    planned.iter().any(|candidate| {
        candidate.place > 0
            && candidate.place < answerer.place
            && (candidate.resting || candidate.nearly_spent || resting(candidate))
    })
}

pub(super) fn least_held(
    mut order: Vec<usize>,
    candidates: &[Candidate],
    held: &Holdings,
    ready: bool,
) -> Vec<usize> {
    if held.is_empty() {
        // nobody holds a conversation in scope: routing's order stands
        return order;
    }
    let mut best = None;
    for (index, candidate) in candidates.iter().enumerate() {
        if candidate.resting || candidate.aside || stands_in(candidates, index) {
            continue;
        }
        if ready && index > 0 && !ready_for_new(candidates, index) {
            continue;
        }
        let count = held.get(&seat(candidate)).copied().unwrap_or(0);
        if best.is_none_or(|best_index| {
            count
                < held
                    .get(&seat(&candidates[best_index]))
                    .copied()
                    .unwrap_or(0)
        }) {
            best = Some(index);
        }
    }
    if let Some(best) = best.filter(|best| *best > 0) {
        let selected = order.remove(best);
        order.insert(0, selected);
    }
    order
}

pub(super) fn after(mut order: Vec<usize>, at: usize, candidates: &[Candidate]) -> Vec<usize> {
    for distance in 1..candidates.len() {
        let index = (at + distance) % candidates.len();
        let candidate = &candidates[index];
        if candidate.resting || candidate.aside || stands_in(candidates, index) {
            continue;
        }
        if index > 0 {
            let selected = order.remove(index);
            order.insert(0, selected);
        }
        break;
    }
    order
}

pub(super) fn ready_for_new(candidates: &[Candidate], index: usize) -> bool {
    let candidate = &candidates[index];
    !candidate.resting
        && !candidate.aside
        && !stands_in(candidates, index)
        && !(candidate.low_allowance && !candidate.week_low)
}

fn stands_in(candidates: &[Candidate], index: usize) -> bool {
    candidates[index].fallback && !candidates[0].fallback
}

fn seat(candidate: &Candidate) -> (String, String, String) {
    (
        candidate.member.clone(),
        candidate.model.clone(),
        candidate.effort.clone(),
    )
}

#[cfg(test)]
mod tests {
    use super::{Candidate, keeps_week_low_routing, ready_for_new};

    fn candidate(low: bool, week_low: bool) -> Candidate {
        Candidate {
            member: "m".to_owned(),
            model: "demo".to_owned(),
            effort: String::new(),
            resting: false,
            reserve: 0,
            nearly_spent: false,
            low_allowance: low,
            week_low,
            fallback: false,
            aside: false,
            place: 1,
            group_member: String::new(),
            cold_after: std::time::Duration::ZERO,
        }
    }

    #[test]
    fn r4_72_week_low_exempts_low_allowance_only_when_flagged() {
        let high = candidate(true, false);
        let candidates = vec![candidate(false, false), high];
        assert!(!ready_for_new(&candidates, 1));

        let kept = candidate(true, true);
        let candidates = vec![candidate(false, false), kept];
        assert!(ready_for_new(&candidates, 1));
    }

    #[test]
    fn r4_72_keeps_week_low_routing_matches_smart_pace_and_empty() {
        for routing in ["", "smart", "pace"] {
            assert!(keeps_week_low_routing(routing), "{routing}");
        }
        for routing in ["usage", "rotate", "weight", "order"] {
            assert!(!keeps_week_low_routing(routing), "{routing}");
        }
    }
}
