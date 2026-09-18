//! The transition table (feedback blocker 1 / M3): every (request-kind ×
//! event-kind) pair and every (request-kind × state-kind) observation is
//! either handled or declared a bug with a reason. The core consults these
//! tables in `apply` before dispatching, so they are load-bearing, not
//! documentation; the tests at the bottom enumerate the full products and
//! fail if any pair is left unclassified.
//!
//! The correlation-token design does most of the work: an event can only
//! reach a state through the token that parked there, so the reachable
//! product is small and each request kind pins its table entry to exactly
//! the listed state kinds.

use crate::events::{EventKind, InflightKind};
use crate::state::{MappingStateKind, RealizationStateKind};

pub enum Classification {
    /// the one success-completion kind for this request
    Expected,
    /// `Cancelled` on a cancellable request (fail-fast drain)
    CancelOk,
    Impossible(&'static str),
}

pub fn expected_event(k: InflightKind) -> EventKind {
    match k {
        InflightKind::MappingQuery => EventKind::MappingAnswered,
        InflightKind::PresenceQuery => EventKind::PresenceAnswered,
        InflightKind::BuildLease | InflightKind::DownloadLease => EventKind::LeaseGranted,
        InflightKind::Build => EventKind::BuildFinished,
        InflightKind::Download => EventKind::DownloadFinished,
        InflightKind::BuildOutputCommit | InflightKind::DownloadCommit => {
            EventKind::OutputCommitted
        }
        InflightKind::BuildMappingCommit | InflightKind::MappingWriteBack => {
            EventKind::MappingCommitted
        }
    }
}

/// Which requests a host may abandon mid-flight. Ack-gated commits never:
/// the store must not be left half-committed.
pub fn cancellable(k: InflightKind) -> bool {
    matches!(
        k,
        InflightKind::MappingQuery
            | InflightKind::PresenceQuery
            | InflightKind::BuildLease
            | InflightKind::Build
            | InflightKind::DownloadLease
            | InflightKind::Download
    )
}

pub fn classify(k: InflightKind, e: EventKind) -> Classification {
    if expected_event(k) == e {
        return Classification::Expected;
    }
    if e == EventKind::Cancelled {
        return if cancellable(k) {
            Classification::CancelOk
        } else {
            Classification::Impossible(
                "host contract: ack-gated commits (and the A12 write-back) are never cancelled",
            )
        };
    }
    Classification::Impossible(
        "correlation: a request token completes only as its expected_event() kind",
    )
}

/// For mapping-keyed requests: the mapping states in which the token may be
/// outstanding when its event (or cancellation) arrives. `None` = the
/// request is realization-keyed.
pub fn legal_mapping_states(k: InflightKind) -> Option<&'static [MappingStateKind]> {
    use MappingStateKind as S;
    Some(match k {
        // a query wave pins the mapping in Querying: cancellations leave the
        // wave incomplete, and decisions happen only on complete waves
        InflightKind::MappingQuery => &[S::Querying],
        InflightKind::BuildLease => &[S::BuildingAwaitingLease],
        InflightKind::Build => &[S::BuildingRunning],
        InflightKind::BuildOutputCommit => &[S::BuildingCommittingOutput],
        InflightKind::BuildMappingCommit => &[S::BuildingCommittingMapping],
        // the mapping is Resolved when the write-back is emitted, but a §8
        // closure-escape on a substituted output can fail it in the meantime
        InflightKind::MappingWriteBack => &[S::Resolved, S::Failed],
        _ => return None,
    })
}

/// For realization-keyed requests. `None` = the request is mapping-keyed.
pub fn legal_realization_states(k: InflightKind) -> Option<&'static [RealizationStateKind]> {
    use RealizationStateKind as S;
    Some(match k {
        // presence answers race against the realization's own progress: a
        // local answer or a finished build can make it Present, and a
        // conflicting refs claim can fail it, while other stores' answers
        // are still outstanding. Downloading is unreachable: downloads only
        // start once the whole wave is in.
        InflightKind::PresenceQuery => &[S::Absent, S::Present, S::Failed],
        InflightKind::DownloadLease => &[S::DownloadingAwaitingLease],
        InflightKind::Download => &[S::DownloadingFetching],
        InflightKind::DownloadCommit => &[S::DownloadingCommitting],
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::{Effect, Event, LeaseId};
    use crate::input::{
        BuilderType, Cores, HumanName, RawInput, RawNode, StoreDef, StoreName, ValidRemoteStores,
    };
    use crate::resolver::Resolver;

    /// M1 acceptance artifact: the full (request × event) product is
    /// classified, each request has exactly one success event, and every
    /// impossible pair carries a reason.
    #[test]
    fn every_request_event_pair_classified() {
        for &k in InflightKind::ALL {
            let mut expected = 0;
            for &e in EventKind::ALL {
                match classify(k, e) {
                    Classification::Expected => {
                        expected += 1;
                        assert_eq!(expected_event(k), e);
                    }
                    Classification::CancelOk => {
                        assert_eq!(e, EventKind::Cancelled);
                        assert!(cancellable(k));
                    }
                    Classification::Impossible(reason) => assert!(!reason.is_empty()),
                }
            }
            assert_eq!(expected, 1, "{k:?} must have exactly one success event");
        }
    }

    /// Every request kind is keyed by exactly one state table, with a
    /// non-empty legal set; everything outside the set is a declared bug
    /// (the core panics on observation).
    #[test]
    fn every_request_pinned_to_one_state_table() {
        for &k in InflightKind::ALL {
            let m = legal_mapping_states(k);
            let r = legal_realization_states(k);
            assert!(
                m.is_some() ^ r.is_some(),
                "{k:?} must be keyed by exactly one table"
            );
            if let Some(states) = m {
                assert!(!states.is_empty());
                assert!(states.iter().all(|s| MappingStateKind::ALL.contains(s)));
            }
            if let Some(states) = r {
                assert!(!states.is_empty());
                assert!(states.iter().all(|s| RealizationStateKind::ALL.contains(s)));
            }
        }
    }

    /// The table is load-bearing: a misdelivered event panics.
    #[test]
    fn misdelivery_panics() {
        let raw = RawInput::from_parts(
            vec![(
                HumanName::new("a").unwrap(),
                RawNode {
                    builder: BuilderType::Process,
                    recipe: b"r".to_vec(),
                    is_target: true,
                    target_store: None,
                    remotes: ValidRemoteStores::None,
                    upstreams: vec![],
                    cores: Cores::One,
                },
            )],
            vec![(
                StoreName::new("p").unwrap(),
                StoreDef::Local { writeable: true },
            )],
        );
        let mut r = Resolver::new(raw.ingest().unwrap());
        let fx = r.start();
        let Effect::QueryMapping { tok, .. } = fx[0] else {
            panic!("expected a mapping query")
        };
        let err = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            r.apply(Event::LeaseGranted {
                tok,
                lease: LeaseId(1),
            });
        }))
        .unwrap_err();
        let msg = err.downcast_ref::<String>().cloned().unwrap_or_default();
        assert!(
            msg.contains("correlation"),
            "panic should cite the table: {msg}"
        );
    }
}
