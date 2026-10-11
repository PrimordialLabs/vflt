//! Choosing the next items for an agent.

use std::collections::HashSet;

use crate::collective::ClaimMode;
use crate::profile::Profile;
use crate::types::{ActorId, Item, ItemId, Status};

/// How an agent is allowed to pick work right now.
#[derive(Debug, Clone, Copy, Default)]
pub struct Picking {
    /// Only items routed here by someone else: skip items this agent filed.
    pub hold: bool,
}

/// Pick the next claimable item for `agent` running `profile`.
pub fn select_next<'a>(
    items: &'a [Item],
    profile: &Profile,
    agent: &ActorId,
    mode: ClaimMode,
    picking: Picking,
) -> Option<&'a Item> {
    select_batch(items, profile, agent, mode, picking, 1)
        .into_iter()
        .next()
}

/// Pick up to `n` claimable items for `agent` running `profile`.
///
/// Rules: status `pending`, not currently claimed, stage handled by the
/// profile, every dependency done or cancelled, and the assignee rule for the
/// collective's claim mode. Lowest priority number first, then oldest.
///
/// An item assigned to the *profile name* counts as delegated to any agent
/// running that profile, so `item add --assign coder` routes to the coder pool.
/// Under a hold, items the agent filed itself are skipped.
pub fn select_batch<'a>(
    items: &'a [Item],
    profile: &Profile,
    agent: &ActorId,
    mode: ClaimMode,
    picking: Picking,
    n: usize,
) -> Vec<&'a Item> {
    let satisfied: HashSet<&ItemId> = items
        .iter()
        .filter(|i| i.status.is_terminal())
        .map(|i| i.id())
        .collect();

    let mut candidates: Vec<&Item> = items
        .iter()
        .filter(|i| i.status == Status::Pending && i.claim.is_none())
        .filter(|i| profile.handles(&i.stage))
        .filter(|i| i.deps.iter().all(|d| satisfied.contains(d)))
        .filter(|i| assignee_ok(i.assignee.as_ref(), agent, profile, mode))
        .filter(|i| !(picking.hold && (&i.owner == agent || i.owner.as_str() == profile.name)))
        .collect();

    candidates.sort_by(|a, b| a.priority.cmp(&b.priority).then(a.created.cmp(&b.created)));
    candidates.truncate(n.max(1));
    candidates
}

/// Is `assignee` one of the names this agent answers to?
pub fn routed_to(assignee: Option<&ActorId>, agent: &ActorId, profile: &Profile) -> bool {
    assignee.is_some_and(|a| a == agent || a.as_str() == profile.name)
}

fn assignee_ok(
    assignee: Option<&ActorId>,
    agent: &ActorId,
    profile: &Profile,
    mode: ClaimMode,
) -> bool {
    let mine = routed_to(assignee, agent, profile);
    match mode {
        ClaimMode::Pull | ClaimMode::Both => assignee.is_none() || mine,
        ClaimMode::Delegated => mine,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::*;
    use chrono::{Duration, Utc};

    fn item(
        id: &str,
        stage: &str,
        prio: i32,
        assignee: Option<&str>,
        deps: &[&str],
        age_s: i64,
    ) -> Item {
        let now = Utc::now() - Duration::seconds(age_s);
        Item {
            state: ItemState {
                id: ItemId::from(id),
                slug: id.into(),
                title: id.into(),
                stage: Stage::new(stage),
                status: Status::Pending,
                priority: prio,
                owner: ActorId::from("t"),
                assignee: assignee.map(ActorId::from),
                tags: vec![],
                deps: deps.iter().map(|d| ItemId::from(*d)).collect(),
                created: now,
                updated: now,
                closed: None,
                external: None,
                waiting_on: None,
                raise_kind: None,
                parent: None,
                origin: None,
                bounces: 0,
                returned: false,
                summary: None,
                key: None,
            },
            spec: String::new(),
            claim: None,
        }
    }

    fn coder() -> Profile {
        Profile::parse("name = \"coder\"\nstages = [\"code\"]\n", "").unwrap()
    }

    #[test]
    fn respects_stage_priority_age_and_deps() {
        let mut done = item("vf-done", "code", 1, None, &[], 100);
        done.state.status = Status::Done;
        let mut draft = item("vf-draft", "code", 0, None, &[], 100);
        draft.state.status = Status::Draft;
        let items = vec![
            item("vf-a", "code", 50, None, &[], 10),
            item("vf-b", "code", 10, None, &[], 5),
            item("vf-c", "code", 10, None, &[], 20), // same prio as b, older -> wins
            item("vf-d", "review", 1, None, &[], 1), // wrong stage
            item("vf-e", "code", 1, None, &["vf-missing"], 1), // unmet dep
            item("vf-f", "code", 100, None, &["vf-done"], 1),
            done,
            draft,
        ];
        let me = ActorId::from("coder-1");
        let pick = select_next(&items, &coder(), &me, ClaimMode::Pull, Picking::default()).unwrap();
        assert_eq!(pick.id().as_str(), "vf-c");
        let batch = select_batch(
            &items,
            &coder(),
            &me,
            ClaimMode::Pull,
            Picking::default(),
            3,
        );
        let ids: Vec<&str> = batch.iter().map(|i| i.id().as_str()).collect();
        assert_eq!(ids, vec!["vf-c", "vf-b", "vf-a"]);
    }

    #[test]
    fn claim_modes_and_profile_routing() {
        let items = vec![
            item("vf-pool", "code", 50, None, &[], 10),
            item("vf-mine", "code", 60, Some("coder-1"), &[], 10),
            item("vf-prof", "code", 70, Some("coder"), &[], 10),
            item("vf-other", "code", 1, Some("coder-2"), &[], 10),
        ];
        let me = ActorId::from("coder-1");
        let p = coder();
        let none = Picking::default();
        assert_eq!(
            select_next(&items, &p, &me, ClaimMode::Pull, none)
                .unwrap()
                .id()
                .as_str(),
            "vf-pool"
        );
        assert_eq!(
            select_next(&items, &p, &me, ClaimMode::Delegated, none)
                .unwrap()
                .id()
                .as_str(),
            "vf-mine"
        );
        let only_prof: Vec<Item> = items
            .iter()
            .filter(|i| i.id().as_str() == "vf-prof")
            .cloned()
            .collect();
        assert_eq!(
            select_next(&only_prof, &p, &me, ClaimMode::Delegated, none)
                .unwrap()
                .id()
                .as_str(),
            "vf-prof"
        );
        let only_other: Vec<Item> = items
            .iter()
            .filter(|i| i.id().as_str() == "vf-other")
            .cloned()
            .collect();
        assert!(select_next(&only_other, &p, &me, ClaimMode::Pull, none).is_none());
    }

    #[test]
    fn hold_skips_self_filed_work() {
        let mut own = item("vf-own", "code", 1, Some("coder-1"), &[], 10);
        own.state.owner = ActorId::from("coder-1");
        let routed = item("vf-routed", "code", 50, Some("coder-1"), &[], 10);
        let items = vec![own, routed];
        let me = ActorId::from("coder-1");
        let held = Picking { hold: true };
        assert_eq!(
            select_next(&items, &coder(), &me, ClaimMode::Delegated, held)
                .unwrap()
                .id()
                .as_str(),
            "vf-routed"
        );
        assert_eq!(
            select_next(
                &items,
                &coder(),
                &me,
                ClaimMode::Delegated,
                Picking::default()
            )
            .unwrap()
            .id()
            .as_str(),
            "vf-own"
        );
    }

    #[test]
    fn claimed_items_are_skipped() {
        let mut it = item("vf-a", "code", 1, None, &[], 1);
        it.claim = Some(Claim {
            item: it.id().clone(),
            actor: ActorId::from("x"),
            at: Utc::now(),
            prior_assignee: None,
        });
        assert!(select_next(
            &[it],
            &coder(),
            &ActorId::from("me"),
            ClaimMode::Pull,
            Picking::default()
        )
        .is_none());
    }
}
