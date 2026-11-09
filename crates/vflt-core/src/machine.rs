//! The item state machine, shared by every store. A store loads the state and
//! the current claim, calls [`apply`], then persists the result atomically in
//! whatever way its backend allows.

use chrono::{DateTime, Utc};
use serde_json::json;

use crate::collective::Collective;
use crate::error::{Error, Result};
use crate::types::*;

/// The outcome of a transition, for the store to persist.
#[derive(Debug, Clone)]
pub struct Applied {
    pub kind: &'static str,
    pub summary: String,
    pub detail: serde_json::Value,
    /// Remove the claim, if one exists.
    pub drop_claim: bool,
    /// A note to append in the same write.
    pub note: Option<String>,
}

/// The actor an item should rest with after its claim goes away: the assignee
/// it had before the claim, else its current one.
pub fn resting_assignee(st: &ItemState, claim: Option<&Claim>) -> Option<ActorId> {
    match claim {
        Some(c) => c.prior_assignee.clone(),
        None => st.assignee.clone(),
    }
}

fn holder_check(id: &ItemId, claim: Option<&Claim>, by: &ActorId) -> Result<()> {
    match claim {
        Some(c) if &c.actor != by => Err(Error::NotClaimHolder {
            item: id.to_string(),
            holder: c.actor.to_string(),
            actor: by.to_string(),
        }),
        _ => Ok(()),
    }
}

/// The origin recorded when a new item is delegated by its filer: a bounce
/// returns it to the filer.
pub fn origin_for_new(
    stage: &Stage,
    owner: &ActorId,
    assignee: Option<&ActorId>,
) -> Option<Origin> {
    match assignee {
        Some(a) if a != owner => Some(Origin {
            stage: stage.clone(),
            assignee: Some(owner.clone()),
        }),
        _ => None,
    }
}

/// Apply `t` to `st`. `claim` is the item's current claim. `Start` from
/// `pending` is not handled here: the store claims first, then applies
/// `Start` to the claimed item.
pub fn apply(
    coll: &Collective,
    st: &mut ItemState,
    claim: Option<&Claim>,
    by: &ActorId,
    t: Transition,
    now: DateTime<Utc>,
) -> Result<Applied> {
    let id = st.id.clone();
    let from = st.status;
    let verb = t.verb();
    let bad = || Error::InvalidTransition {
        item: id.to_string(),
        verb,
        from: from.to_string(),
    };
    let applied = match t {
        Transition::Start => {
            if from != Status::Claimed {
                return Err(bad());
            }
            holder_check(&id, claim, by)?;
            st.status = Status::InProgress;
            Applied {
                kind: "item.started",
                summary: format!("{by} started {id}"),
                detail: serde_json::Value::Null,
                drop_claim: false,
                note: None,
            }
        }
        Transition::Complete {
            bounce,
            to_stage,
            note,
        } => {
            if !matches!(from, Status::InProgress | Status::Claimed) {
                return Err(bad());
            }
            holder_check(&id, claim, by)?;
            let prev_stage = st.stage.clone();
            let resting = resting_assignee(st, claim);
            st.waiting_on = None;
            st.raise_kind = None;
            if bounce {
                let (stage, assignee) = match st.origin.take() {
                    Some(o) => (o.stage, o.assignee),
                    None => (coll.bounce_to.clone(), None),
                };
                st.stage = stage;
                st.assignee = assignee;
                st.status = Status::Pending;
                st.bounces += 1;
                st.returned = true;
                Applied {
                    kind: "item.bounced",
                    summary: format!(
                        "{by} bounced {id} from {prev_stage} back to {}{}",
                        st.stage,
                        st.assignee
                            .as_ref()
                            .map(|a| format!(" ({a})"))
                            .unwrap_or_default()
                    ),
                    detail: json!({"from_stage": prev_stage, "to_stage": st.stage, "to": st.assignee, "bounce": true}),
                    drop_claim: true,
                    note,
                }
            } else {
                let target = match to_stage {
                    Some(s) => Some(s),
                    None => coll.next_stage(&st.stage).cloned(),
                };
                match target {
                    Some(s) if !coll.is_terminal_stage(&s) => {
                        st.origin = Some(Origin {
                            stage: prev_stage.clone(),
                            assignee: resting,
                        });
                        st.stage = s;
                        st.status = Status::Pending;
                        st.assignee = None;
                        Applied {
                            kind: "item.completed",
                            summary: format!(
                                "{by} completed {id} at {prev_stage}; now pending at {}",
                                st.stage
                            ),
                            detail: json!({"from_stage": prev_stage, "to_stage": st.stage, "bounce": false}),
                            drop_claim: true,
                            note,
                        }
                    }
                    _ => {
                        st.stage = coll.terminal_stage().clone();
                        st.status = Status::Done;
                        st.closed = Some(now);
                        Applied {
                            kind: "item.completed",
                            summary: format!("{by} completed {id} at {prev_stage}; done"),
                            detail: json!({"from_stage": prev_stage, "to_stage": st.stage, "bounce": false}),
                            drop_claim: true,
                            note,
                        }
                    }
                }
            }
        }
        Transition::Block { on } => {
            if !matches!(from, Status::Pending | Status::InProgress | Status::Claimed) {
                return Err(bad());
            }
            holder_check(&id, claim, by)?;
            st.status = Status::Blocked;
            st.waiting_on = Some(on.clone());
            Applied {
                kind: "item.blocked",
                summary: format!("{by} blocked {id} on: {on}"),
                detail: json!({"on": on}),
                drop_claim: true,
                note: None,
            }
        }
        Transition::Raise { question, kind } => {
            if from.is_terminal() {
                return Err(bad());
            }
            holder_check(&id, claim, by)?;
            st.status = Status::NeedsHuman;
            st.waiting_on = Some(question.clone());
            st.raise_kind = Some(kind);
            Applied {
                kind: "item.raised",
                summary: format!(
                    "{by} raised {id} for a human ({}): {question}",
                    kind.as_str()
                ),
                detail: json!({"question": question, "raise_kind": kind}),
                drop_claim: true,
                note: None,
            }
        }
        Transition::Handoff { to: target, stage } => {
            if from.is_terminal() {
                return Err(bad());
            }
            holder_check(&id, claim, by)?;
            let resting = resting_assignee(st, claim);
            // Handing an item to whoever already holds it, at the same stage,
            // is a re-queue, not a hand-over: the origin stays, so a later
            // bounce still returns it to whoever sent it here.
            let requeue = resting.as_ref() == Some(&target)
                && stage.as_ref().map_or(true, |s| s == &st.stage);
            if !requeue {
                st.origin = Some(Origin {
                    stage: st.stage.clone(),
                    assignee: resting,
                });
            }
            // A draft handed on stays a draft: routing never releases it.
            if from != Status::Draft {
                st.status = Status::Pending;
            }
            st.assignee = Some(target.clone());
            st.waiting_on = None;
            st.raise_kind = None;
            if let Some(s) = stage {
                st.stage = s;
            }
            Applied {
                kind: "item.handoff",
                summary: format!("{by} handed {id} to {target} at {}", st.stage),
                detail: json!({"to": target, "stage": st.stage}),
                drop_claim: true,
                note: None,
            }
        }
        Transition::Wait { on } => {
            if !matches!(from, Status::Pending | Status::InProgress | Status::Claimed) {
                return Err(bad());
            }
            if on.is_empty() {
                return Err(Error::invalid("wait", "name at least one item to wait on"));
            }
            holder_check(&id, claim, by)?;
            st.assignee = resting_assignee(st, claim);
            for d in &on {
                if d == &st.id {
                    return Err(Error::invalid(
                        "wait",
                        format!("{id} cannot wait on itself"),
                    ));
                }
                if !st.deps.contains(d) {
                    st.deps.push(d.clone());
                }
            }
            st.status = Status::Pending;
            Applied {
                kind: "item.waiting",
                summary: format!(
                    "{by} parked {id} until {} done",
                    on.iter().map(|d| d.as_str()).collect::<Vec<_>>().join(", ")
                ),
                detail: json!({"on": on}),
                drop_claim: true,
                note: None,
            }
        }
        Transition::Reopen { to } => {
            if !matches!(from, Status::Blocked | Status::NeedsHuman) {
                return Err(bad());
            }
            st.status = Status::Pending;
            st.waiting_on = None;
            st.raise_kind = None;
            if let Some(a) = &to {
                st.assignee = Some(a.clone());
            }
            Applied {
                kind: "item.reopened",
                summary: match &to {
                    Some(a) => format!("{by} reopened {id} for {a}"),
                    None => format!("{by} reopened {id}"),
                },
                detail: json!({"to": to}),
                drop_claim: false,
                note: None,
            }
        }
        Transition::Promote => {
            if from != Status::Draft {
                return Err(bad());
            }
            st.status = Status::Pending;
            Applied {
                kind: "item.promoted",
                summary: format!("{by} promoted {id} from draft"),
                detail: serde_json::Value::Null,
                drop_claim: false,
                note: None,
            }
        }
        Transition::Demote => {
            if from != Status::Pending || claim.is_some() {
                return Err(bad());
            }
            st.status = Status::Draft;
            Applied {
                kind: "item.demoted",
                summary: format!("{by} returned {id} to draft"),
                detail: serde_json::Value::Null,
                drop_claim: false,
                note: None,
            }
        }
        Transition::Cancel => {
            if from.is_terminal() {
                return Err(bad());
            }
            st.status = Status::Cancelled;
            st.closed = Some(now);
            st.raise_kind = None;
            Applied {
                kind: "item.cancelled",
                summary: format!("{by} cancelled {id}"),
                detail: serde_json::Value::Null,
                drop_claim: true,
                note: None,
            }
        }
    };
    st.updated = now;
    // Any move other than a bounce means someone has taken the item on.
    if applied.kind != "item.bounced" {
        st.returned = false;
    }
    let mut applied = applied;
    let mut detail = match applied.detail.take() {
        serde_json::Value::Object(m) => m,
        _ => serde_json::Map::new(),
    };
    detail.insert("from_status".into(), json!(from));
    detail.insert("to_status".into(), json!(st.status));
    applied.detail = serde_json::Value::Object(detail);
    Ok(applied)
}

/// Would adding `new_deps` to `id` create a cycle? `deps_of` returns the
/// dependencies of an item.
pub fn creates_cycle(
    id: &ItemId,
    new_deps: &[ItemId],
    mut deps_of: impl FnMut(&ItemId) -> Result<Vec<ItemId>>,
) -> Result<bool> {
    let mut stack: Vec<ItemId> = new_deps.to_vec();
    let mut seen = std::collections::HashSet::new();
    while let Some(d) = stack.pop() {
        if &d == id {
            return Ok(true);
        }
        if !seen.insert(d.clone()) {
            continue;
        }
        stack.extend(deps_of(&d)?);
    }
    Ok(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn st(stage: &str, status: Status) -> ItemState {
        let now = Utc::now();
        ItemState {
            id: ItemId::from("vf-1"),
            slug: "x".into(),
            title: "x".into(),
            stage: Stage::new(stage),
            status,
            priority: 50,
            owner: ActorId::from("lead"),
            assignee: None,
            tags: vec![],
            deps: vec![],
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
        }
    }

    fn claim(by: &str, prior: Option<&str>) -> Claim {
        Claim {
            item: ItemId::from("vf-1"),
            actor: ActorId::from(by),
            at: Utc::now(),
            prior_assignee: prior.map(ActorId::from),
        }
    }

    #[test]
    fn bounce_returns_to_origin_then_falls_back_to_bounce_stage() {
        let coll = Collective::new("t");
        let mut s = st("code", Status::InProgress);
        // code -> review records the origin (the pre-claim assignee)
        let c = claim("coder-1", Some("coder"));
        apply(
            &coll,
            &mut s,
            Some(&c),
            &ActorId::from("coder-1"),
            Transition::Complete {
                bounce: false,
                to_stage: None,
                note: None,
            },
            Utc::now(),
        )
        .unwrap();
        assert_eq!(s.stage.as_str(), "review");
        assert_eq!(
            s.origin
                .as_ref()
                .unwrap()
                .assignee
                .as_ref()
                .unwrap()
                .as_str(),
            "coder"
        );
        s.status = Status::InProgress;
        let r = claim("rev", None);
        let a = apply(
            &coll,
            &mut s,
            Some(&r),
            &ActorId::from("rev"),
            Transition::Complete {
                bounce: true,
                to_stage: None,
                note: Some("nit".into()),
            },
            Utc::now(),
        )
        .unwrap();
        assert_eq!(a.kind, "item.bounced");
        assert_eq!(s.stage.as_str(), "code");
        assert_eq!(s.assignee.as_ref().unwrap().as_str(), "coder");
        assert_eq!(s.bounces, 1);
        assert!(s.returned);
        assert_eq!(a.detail["from_status"], "in_progress");
        assert_eq!(a.detail["to_status"], "pending");
        assert!(s.origin.is_none());
        // no origin: the collective's bounce stage, back to the pool
        let mut t = st("test", Status::InProgress);
        apply(
            &coll,
            &mut t,
            None,
            &ActorId::from("x"),
            Transition::Complete {
                bounce: true,
                to_stage: None,
                note: None,
            },
            Utc::now(),
        )
        .unwrap();
        assert_eq!(t.stage.as_str(), "code");
        assert!(t.assignee.is_none());
    }

    #[test]
    fn a_self_requeue_keeps_the_origin_for_a_later_bounce() {
        let coll = Collective::new("t");
        let mut s = st("work", Status::InProgress);
        s.assignee = Some(ActorId::from("tester"));
        s.origin = origin_for_new(
            &s.stage,
            &ActorId::from("lead"),
            Some(&ActorId::from("tester")),
        );
        let tester = ActorId::from("tester");
        apply(
            &coll,
            &mut s,
            None,
            &tester,
            Transition::Handoff {
                to: tester.clone(),
                stage: None,
            },
            Utc::now(),
        )
        .unwrap();
        assert_eq!(s.status, Status::Pending);
        s.status = Status::InProgress;
        apply(
            &coll,
            &mut s,
            None,
            &tester,
            Transition::Complete {
                bounce: true,
                to_stage: None,
                note: None,
            },
            Utc::now(),
        )
        .unwrap();
        assert_eq!(s.assignee.as_ref().unwrap().as_str(), "lead");
    }

    #[test]
    fn delegated_items_bounce_to_their_filer() {
        let coll = Collective::new("t");
        let mut s = st("work", Status::InProgress);
        s.origin = origin_for_new(
            &s.stage,
            &ActorId::from("lead"),
            Some(&ActorId::from("tester")),
        );
        apply(
            &coll,
            &mut s,
            None,
            &ActorId::from("tester"),
            Transition::Complete {
                bounce: true,
                to_stage: None,
                note: None,
            },
            Utc::now(),
        )
        .unwrap();
        assert_eq!(s.assignee.as_ref().unwrap().as_str(), "lead");
        assert_eq!(s.stage.as_str(), "work");
        assert_eq!(s.status, Status::Pending);
        assert!(origin_for_new(&s.stage, &ActorId::from("a"), Some(&ActorId::from("a"))).is_none());
    }

    #[test]
    fn wait_parks_with_deps_and_restores_resting_assignee() {
        let coll = Collective::new("t");
        let mut s = st("code", Status::InProgress);
        s.assignee = Some(ActorId::from("coder-1"));
        let c = claim("coder-1", Some("coder"));
        apply(
            &coll,
            &mut s,
            Some(&c),
            &ActorId::from("coder-1"),
            Transition::Wait {
                on: vec![ItemId::from("vf-2")],
            },
            Utc::now(),
        )
        .unwrap();
        assert_eq!(s.status, Status::Pending);
        assert_eq!(s.deps, vec![ItemId::from("vf-2")]);
        assert_eq!(s.assignee.as_ref().unwrap().as_str(), "coder");
        assert!(apply(
            &coll,
            &mut s,
            None,
            &ActorId::from("x"),
            Transition::Wait {
                on: vec![ItemId::from("vf-1")]
            },
            Utc::now()
        )
        .is_err());
    }

    #[test]
    fn drafts_promote_demote_and_survive_handoff() {
        let coll = Collective::new("t");
        let mut s = st("code", Status::Draft);
        apply(
            &coll,
            &mut s,
            None,
            &ActorId::from("x"),
            Transition::Handoff {
                to: ActorId::from("y"),
                stage: None,
            },
            Utc::now(),
        )
        .unwrap();
        assert_eq!(s.status, Status::Draft);
        apply(
            &coll,
            &mut s,
            None,
            &ActorId::from("x"),
            Transition::Promote,
            Utc::now(),
        )
        .unwrap();
        assert_eq!(s.status, Status::Pending);
        apply(
            &coll,
            &mut s,
            None,
            &ActorId::from("x"),
            Transition::Demote,
            Utc::now(),
        )
        .unwrap();
        assert_eq!(s.status, Status::Draft);
        assert!(apply(
            &coll,
            &mut s,
            None,
            &ActorId::from("x"),
            Transition::Demote,
            Utc::now()
        )
        .is_err());
    }

    #[test]
    fn raise_records_kind_and_reopen_can_reroute() {
        let coll = Collective::new("t");
        let mut s = st("code", Status::InProgress);
        apply(
            &coll,
            &mut s,
            None,
            &ActorId::from("a"),
            Transition::Raise {
                question: "which?".into(),
                kind: RaiseKind::Action,
            },
            Utc::now(),
        )
        .unwrap();
        assert_eq!(s.raise_kind, Some(RaiseKind::Action));
        apply(
            &coll,
            &mut s,
            None,
            &ActorId::from("h"),
            Transition::Reopen {
                to: Some(ActorId::from("b")),
            },
            Utc::now(),
        )
        .unwrap();
        assert_eq!(s.status, Status::Pending);
        assert_eq!(s.raise_kind, None);
        assert_eq!(s.assignee.as_ref().unwrap().as_str(), "b");
    }

    #[test]
    fn non_holders_are_refused() {
        let coll = Collective::new("t");
        let mut s = st("code", Status::InProgress);
        let c = claim("a", None);
        assert!(matches!(
            apply(
                &coll,
                &mut s,
                Some(&c),
                &ActorId::from("b"),
                Transition::Complete {
                    bounce: false,
                    to_stage: None,
                    note: None
                },
                Utc::now()
            ),
            Err(Error::NotClaimHolder { .. })
        ));
    }

    #[test]
    fn cycles_are_detected() {
        let deps = |id: &ItemId| -> Result<Vec<ItemId>> {
            Ok(match id.as_str() {
                "vf-2" => vec![ItemId::from("vf-3")],
                "vf-3" => vec![ItemId::from("vf-1")],
                _ => vec![],
            })
        };
        assert!(creates_cycle(&ItemId::from("vf-1"), &[ItemId::from("vf-2")], deps).unwrap());
        assert!(!creates_cycle(&ItemId::from("vf-1"), &[ItemId::from("vf-4")], deps).unwrap());
    }
}
