//! Event intake, not an event source or notification owner. A Git hook or
//! repository observer supplies an occurrence; routing reads current facts.

use anyhow::{bail, Result};
use triblespace::core::metadata;
use triblespace::prelude::*;

use super::model;
use super::operations::{Execution, ExecutionOptions, Triggers};
use super::runner::Context;
use crate::schemas::trigger::{attrs, KIND_TRIGGER};

impl Triggers {
    /// Route one caller-owned occurrence to the live definitions interested
    /// in its exact event/context. Timers are never selected by this path.
    ///
    /// Redelivery keeps the occurrence ID and reuses persisted run results.
    /// A new filesystem notification is not necessarily a new condition:
    /// condition checks still own onset/resolution and disposition semantics.
    /// This is not a distributed execution lock; the caller serializes intake.
    pub fn dispatch_event(
        &self,
        event: &str,
        persona: Id,
        context: Context,
        occurrence: Id,
        options: ExecutionOptions<'_>,
    ) -> Result<Vec<Execution>> {
        if event.is_empty() || event.len() > 30 || context == Context::Timer {
            bail!("event intake needs a name of 1..=30 bytes and an explicit event context");
        }
        let definitions = self.with_facts(true, |_, _, _, _, facts| {
            Ok(find!(definition: Id, pattern!(facts, [{ ?definition @
                metadata::tag: &KIND_TRIGGER,
                attrs::event: event,
                attrs::context: context.as_str(),
            }]))
            .filter(|definition| {
                model::current(facts, *definition)
                    && model::active(facts, *definition)
                    && model::addressed_to(facts, *definition, persona)
            })
            .collect::<Vec<_>>())
        })?;
        let mut executions = Vec::new();
        for definition in definitions {
            executions.extend(self.run_event(
                definition,
                persona,
                context,
                Some(event),
                Some(occurrence),
                options,
            )?);
        }
        Ok(executions)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn intake_routes_only_live_addressed_events_and_redelivery_does_not_execute() {
        let directory = tempfile::tempdir().unwrap();
        let pile = directory.path().join("events.pile");
        let key = directory.path().join("events.key");
        std::fs::File::create(&pile).unwrap();
        crate::storage::initialize_signer(&pile, Some(&key)).unwrap();
        let triggers = Triggers::new(pile.clone(), Some(key.clone()));
        let persona = *triblespace::core::id::genid();
        let other_persona = *triblespace::core::id::genid();
        let options = ExecutionOptions {
            directory: directory.path(),
            stdin: b"",
            timeout: Duration::from_secs(3),
            output_limit: 4096,
        };
        let event = |label, command, name, context, personas: &[Id]| {
            triggers
                .publish(
                    model::event_definition(label, command, name, context, personas, &[]).unwrap(),
                )
                .unwrap()
        };
        let old = event(
            "old",
            "touch unexpected",
            "repo-refs-changed",
            Context::AdvisoryEvent,
            &[],
        );
        let active = triggers
            .publish(
                model::event_definition(
                    "current",
                    "printf x >> calls; printf checked",
                    "repo-refs-changed",
                    Context::AdvisoryEvent,
                    &[persona],
                    &[old],
                )
                .unwrap(),
            )
            .unwrap();
        let paused = event(
            "paused",
            "touch unexpected",
            "repo-refs-changed",
            Context::AdvisoryEvent,
            &[],
        );
        triggers
            .publish(model::state_fragment(paused, true, &[]))
            .unwrap();
        event(
            "different-event",
            "touch unexpected",
            "post-commit",
            Context::AdvisoryEvent,
            &[],
        );
        event(
            "different-context",
            "touch unexpected",
            "repo-refs-changed",
            Context::SynchronousEvent,
            &[],
        );
        event(
            "different-persona",
            "touch unexpected",
            "repo-refs-changed",
            Context::AdvisoryEvent,
            &[other_persona],
        );
        triggers
            .publish(
                model::timer_definition(
                    "timer",
                    "touch unexpected",
                    60,
                    hifitime::Epoch::from_unix_seconds(0.0),
                    &[],
                    &[],
                )
                .unwrap(),
            )
            .unwrap();
        let occurrence = *triblespace::core::id::genid();
        // Event names are multi-valued. Routing this name must not execute
        // the same command again under an unrelated name on the definition.
        triggers
            .publish(entity! { ExclusiveId::force_ref(&active) @
                attrs::event: "post-commit",
            })
            .unwrap();
        let first = triggers
            .dispatch_event(
                "repo-refs-changed",
                persona,
                Context::AdvisoryEvent,
                occurrence,
                options,
            )
            .unwrap();
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].trigger, active);
        assert_eq!(first[0].outcome.stdout, b"checked");
        drop(triggers);
        let reopened = Triggers::new(pile, Some(key));
        let replay = reopened
            .dispatch_event(
                "repo-refs-changed",
                persona,
                Context::AdvisoryEvent,
                occurrence,
                options,
            )
            .unwrap();
        assert_eq!(replay.len(), 1);
        assert_eq!(replay[0].run, first[0].run);
        assert_eq!(replay[0].result, first[0].result);
        assert_eq!(std::fs::read(directory.path().join("calls")).unwrap(), b"x");
        assert!(!directory.path().join("unexpected").exists());
        let next = reopened
            .dispatch_event(
                "repo-refs-changed",
                persona,
                Context::AdvisoryEvent,
                *triblespace::core::id::genid(),
                options,
            )
            .unwrap();
        assert_ne!(next[0].run, first[0].run);
        assert_eq!(
            std::fs::read(directory.path().join("calls")).unwrap(),
            b"xx"
        );
    }
}
