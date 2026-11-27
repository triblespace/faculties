use super::*;

fn snapshot(id: Id, outcome: decisions::TextHandle, forced: bool) -> ResolutionSnapshot {
    ResolutionSnapshot {
        id,
        decision: genid().id,
        outcome,
        result: None,
        forced,
        evidence: Vec::new(),
        predecessors: Vec::new(),
        finished_at: epoch_interval(Epoch::from_unix_seconds(1.0)),
    }
}

#[test]
fn ordinary_actions_accept_only_missing_resolution() {
    let decision = genid().id;
    assert!(ensure_missing(&Resolution::Missing, "act", decision).is_ok());
    assert!(ensure_missing(&Resolution::Invalid("bad".into()), "act", decision).is_err());
}

#[test]
fn reconciliation_accepts_only_genuine_forks_and_keeps_every_head() {
    let decision = genid().id;
    let first = genid().id;
    let second = genid().id;
    let handle = Inline::new([0x11; 32]);
    let heads = reconciliation_heads(
        Resolution::Forked(vec![
            snapshot(first, handle, false),
            snapshot(second, handle, true),
        ]),
        decision,
    )
    .unwrap();
    assert_eq!(heads, vec![first, second]);
    assert!(reconciliation_heads(
        Resolution::Agreed(vec![
            snapshot(first, handle, false),
            snapshot(second, handle, false),
        ]),
        decision,
    )
    .is_err());
}

/// A foundation of the collection is stranded on another host: a proposal is
/// written and the reads answer over what is here, naming it; a factor and a
/// resolution, which refuse to follow a resolution they cannot see, refuse
/// the incomplete read.
#[test]
fn reads_and_proposals_go_on_while_a_decisions_foundation_is_away() {
    use crate::storage::residual_fixture::{
        refuses_incomplete, storage, strand_in, unrelated_fragment,
    };
    let directory = tempfile::tempdir().unwrap();
    let storage = storage(directory.path());
    let decide = Decisions::with_storage(storage.clone());
    let resident = decide.propose("resident question", None, None).unwrap();
    assert!(resident.notes.is_empty(), "{:?}", resident.notes);
    let away = strand_in(&storage, DEFAULT_SCOPE_ID, unrelated_fragment());
    let named = |notes: &[String]| notes.iter().any(|note| note.contains(&away));
    let id = format!("{:x}", resident.decision);

    let proposed = decide.propose("meanwhile", None, None).unwrap();
    assert!(named(&proposed.notes), "{:?}", proposed.notes);
    let listed = decide
        .list(ListOptions {
            all: true,
            forced: false,
        })
        .unwrap();
    assert_eq!(listed.len(), 2);
    assert!(named(&listed.notes));
    assert!(named(&decide.show(&id).unwrap().notes));
    assert!(named(&decide.resolve_id(&id).unwrap().notes));
    let error = decide.show(&format!("{:x}", genid().id)).unwrap_err();
    assert!(format!("{error:#}").contains(&away), "{error:#}");

    let refused = |error: anyhow::Error| assert!(refuses_incomplete(&error, &away), "{error:#}");
    refused(decide.factor(&id, "a reason", FactorSide::Pro).unwrap_err());
    refused(decide.resolve(&id, "done", None, true).unwrap_err());
    storage.close().unwrap();
}

#[test]
fn decisions_name_raw_pending_but_reject_malformed_selected_attachments() {
    use crate::storage::residual_fixture::{
        is_fault, malformed_attachment_in, malformed_in, storage,
    };
    let directory = tempfile::tempdir().unwrap();
    let storage = storage(directory.path());
    let decide = Decisions::with_storage(storage.clone());
    let pending = malformed_in(&storage, DEFAULT_SCOPE_ID);
    let listed = decide
        .list(ListOptions {
            all: true,
            forced: false,
        })
        .unwrap();
    assert!(listed.notes.iter().any(|note| note.contains(&pending)));
    let fault = |error: anyhow::Error| assert!(is_fault(&error), "{error:#}");
    malformed_attachment_in(&storage, DEFAULT_SCOPE_ID);
    fault(
        decide
            .list(ListOptions {
                all: true,
                forced: false,
            })
            .unwrap_err(),
    );
    fault(decide.propose("q", None, None).unwrap_err());
    storage.close().unwrap();
}
