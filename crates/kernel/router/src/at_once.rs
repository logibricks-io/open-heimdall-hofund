//! Accounts-at-once placement after a routing strategy orders candidates.

use std::collections::BTreeMap;

/// One member's provider or subscription limit and configured position.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AtOnceMember {
    /// Provider or subscription whose members share the limit.
    pub group: String,
    /// How many members take requests; zero means no limit.
    pub limit: usize,
    /// Position in the user's own member order.
    pub rank: usize,
}

pub(crate) struct Placement {
    pub order: Vec<usize>,
    pub reserves: BTreeMap<usize, usize>,
}

pub(crate) fn place(
    members: &BTreeMap<String, AtOnceMember>,
    ids: &[String],
    ordered: &[usize],
    resting: &[bool],
) -> Placement {
    let mut kept = vec![true; ids.len()];
    let mut groups = Vec::<&str>::new();
    for id in ids {
        let Some(member) = members.get(id).filter(|member| member.limit > 0) else {
            continue;
        };
        if !groups.contains(&member.group.as_str()) {
            groups.push(&member.group);
        }
    }
    let mut reserves = BTreeMap::new();
    for group in &groups {
        let mut ranked = ids
            .iter()
            .enumerate()
            .filter(|(_, id)| {
                members
                    .get(*id)
                    .is_some_and(|member| member.limit > 0 && member.group == *group)
            })
            .map(|(index, _)| index)
            .collect::<Vec<_>>();
        let limit = members[&ids[ranked[0]]].limit;
        if ranked.len() <= limit {
            continue;
        }
        ranked.sort_by_key(|index| members[&ids[*index]].rank);
        let mut seats = 0;
        for prefer_resting in [false, true] {
            for index in ranked.iter().copied() {
                if seats < limit && resting[index] == prefer_resting && kept[index] {
                    seats += 1;
                    kept[index] = false;
                }
            }
        }
        for index in ranked {
            if kept[index] {
                reserves.insert(index, limit);
            }
        }
    }
    let mut order = ordered
        .iter()
        .copied()
        .filter(|index| !reserves.contains_key(index))
        .collect::<Vec<_>>();
    let mut ranked_reserves = reserves.keys().copied().collect::<Vec<_>>();
    ranked_reserves.sort_by_key(|index| {
        let member = &members[&ids[*index]];
        (
            groups.iter().position(|group| *group == member.group),
            member.rank,
        )
    });
    order.extend(ranked_reserves);
    Placement { order, reserves }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::{AtOnceMember, place};
    use crate::strategy::{Seat, StrategyState};

    fn fixture() -> (BTreeMap<String, AtOnceMember>, Vec<String>, Vec<bool>) {
        let members = ["first", "third", "second"]
            .into_iter()
            .enumerate()
            .map(|(index, id)| {
                (
                    id.to_owned(),
                    AtOnceMember {
                        group: "provider".to_owned(),
                        limit: 2,
                        rank: [1, 0, 2][index],
                    },
                )
            })
            .collect::<BTreeMap<_, _>>();
        let ids = ["first", "third", "second"].map(str::to_owned).to_vec();
        (members, ids, vec![false; 3])
    }

    fn keys<'a>(ids: &'a [String], order: &[usize]) -> Vec<&'a str> {
        order.iter().map(|index| ids[*index].as_str()).collect()
    }

    #[test]
    fn d26_3_every_strategy_keeps_first_in_user_order_and_reserves_last() {
        let (members, ids, resting) = fixture();
        let seats = ids.iter().map(Seat::new).collect::<Vec<_>>();
        let state = StrategyState::with_builtins();
        for mode in [
            "smart", "order", "rotate", "usage", "pace", "weight", "free", "fast", "cheap",
        ] {
            let strategy = state.order(mode, "at-once-test", &seats);
            let placement = place(&members, &ids, &strategy, &resting);
            let routed = keys(&ids, &placement.order);
            assert_eq!(routed.len(), 3, "{mode}");
            assert_eq!(routed[2], "second", "{mode}: {routed:?}");
            assert!(routed[..2].contains(&"first"), "{mode}");
            assert!(routed[..2].contains(&"third"), "{mode}");
            assert_eq!(placement.reserves, BTreeMap::from([(2, 2)]), "{mode}");
            let strategy_keys = keys(&ids, &strategy);
            let kept = routed[..2].to_vec();
            assert_eq!(
                kept,
                strategy_keys
                    .into_iter()
                    .filter(|id| *id != "second")
                    .collect::<Vec<_>>(),
                "{mode} changed the kept candidates' strategy order"
            );
        }
        let unlimited = BTreeMap::new();
        let placement = place(&unlimited, &ids, &[2, 1, 0], &resting);
        assert_eq!(placement.order, [2, 1, 0]);
        assert!(placement.reserves.is_empty());
    }

    #[test]
    fn d26_4_resting_kept_member_gives_seat_to_first_reserve_and_takes_it_back() {
        let (members, ids, mut resting) = fixture();
        resting[1] = true;
        let placement = place(&members, &ids, &[0, 1, 2], &resting);
        assert_eq!(keys(&ids, &placement.order), ["first", "second", "third"]);
        assert_eq!(placement.reserves, BTreeMap::from([(1, 2)]));
        resting[1] = false;
        let placement = place(&members, &ids, &[0, 1, 2], &resting);
        assert_eq!(keys(&ids, &placement.order), ["first", "third", "second"]);
        assert_eq!(placement.reserves, BTreeMap::from([(2, 2)]));
    }
}
