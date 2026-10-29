//! One report's callback outcome and opt-in, pre-submission retry contract.
//!
//! No queue or persistent retry state: the caller retains its existing report
//! until acceptance or cancellation. Observers must not re-enter the owning
//! Faculty store (its storage guard is held), block, or decide delivery policy.

use anyhow::{Context, Result};
use std::fmt;
use std::time::{Duration, Instant};

pub const KNOWN_NOT_SUBMITTED_EXIT: u8 = 75;

/// A trusted callback explicitly promises it submitted no accepting request.
#[derive(Debug)]
pub struct KnownNotSubmitted;

impl fmt::Display for KnownNotSubmitted {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("callback reports known not submitted (exit 75)")
    }
}
impl std::error::Error for KnownNotSubmitted {}

/// One known-unsent deferral, observed by the owning operation before sleep.
/// The reason is a fixed contract label, never callback stderr or report data.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RetryNotice {
    pub attempt: u64,
    pub reason: &'static str,
    pub delay: Duration,
}

/// A retry does not mean a new observation. Accepted means complete output
/// was accepted, not handled; a later receipt error stays fatal, without retry.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DeliveryNotice {
    Retry(RetryNotice),
    Accepted,
}

/// Explicit run-for expiry without an in-flight accepting attempt.
#[derive(Debug)]
pub(crate) struct DeliveryDeadline;
impl fmt::Display for DeliveryDeadline {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Orient run-for expired with a report still not submitted")
    }
}
impl std::error::Error for DeliveryDeadline {}

#[derive(Debug)]
struct DeferredNotSubmitted {
    notice: RetryNotice,
    wake_at: Instant,
    deadline: Option<Instant>,
}
impl fmt::Display for DeferredNotSubmitted {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("known-not-submitted callback deferred within the same report")
    }
}
impl std::error::Error for DeferredNotSubmitted {}

/// Ephemeral control for the sole callback sink, reset after each accepted
/// report. The 5/20/60 state saturates; it retains no report or event catalogue.
pub(crate) struct RetryPolicy {
    enabled: bool,
    backoff_step: u8,
    attempt: u64,
    deadline: Option<Instant>,
}

impl RetryPolicy {
    pub(crate) fn new(enabled: bool, deadline: Option<Instant>) -> Self {
        Self { enabled, backoff_step: 0, attempt: 0, deadline }
    }

    pub(crate) fn attempt_timeout(&self, configured: Duration) -> Result<Duration> {
        match self.deadline {
            Some(deadline) => {
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    return Err(DeliveryDeadline.into());
                }
                Ok(configured.min(remaining))
            }
            None => Ok(configured),
        }
    }

    pub(crate) fn observed(
        &mut self,
        result: Result<()>,
    ) -> Result<()> {
        self.attempt = self.attempt.saturating_add(1);
        let known_unsent = matches!(&result, Err(error) if error.is::<KnownNotSubmitted>());
        let expired = self.deadline.is_some_and(|deadline| deadline <= Instant::now());
        let retry_after = if self.enabled && !expired && known_unsent {
            let delay = match self.backoff_step { 0 => 5, 1 => 20, _ => 60 };
            self.backoff_step = self.backoff_step.saturating_add(1).min(2);
            Some(Duration::from_secs(delay))
        } else {
            self.backoff_step = 0;
            None
        };
        if self.enabled && expired && known_unsent {
            return Err(DeliveryDeadline.into());
        }
        if let Some(delay) = retry_after {
            let now = Instant::now();
            if self.deadline.is_some_and(|deadline| deadline <= now) {
                return Err(DeliveryDeadline.into());
            }
            return Err(DeferredNotSubmitted {
                notice: RetryNotice {
                    attempt: self.attempt,
                    reason: "callback_exit_75_known_not_submitted",
                    delay,
                },
                wake_at: now + delay,
                deadline: self.deadline,
            }.into());
        }
        self.attempt = 0;
        result
    }
}

/// Await backoff inside the existing daemon runtime, not in the synchronous
/// callback process loop. This yields to the daemon's SIGINT/SIGTERM selector.
/// The same report bytes are re-emitted; no receipt can be published on Err.
pub(crate) async fn complete_report(
    output: &mut crate::out::Out<'_>,
    report: &str,
    description: &str,
    on_delivery: &mut dyn FnMut(DeliveryNotice),
) -> Result<()> {
    loop {
        let error = match output.text(report) {
            Ok(()) => {
                on_delivery(DeliveryNotice::Accepted);
                return Ok(());
            }
            Err(error) => error,
        };
        let Some(deferred) = error.downcast_ref::<DeferredNotSubmitted>() else {
            return Err(error).with_context(|| format!("deliver complete {description}"));
        };
        on_delivery(DeliveryNotice::Retry(deferred.notice));
        let wake_at = deferred.deadline.map_or(deferred.wake_at, |end| end.min(deferred.wake_at));
        tokio::time::sleep_until(tokio::time::Instant::from_std(wake_at)).await;
        if deferred.deadline.is_some_and(|deadline| Instant::now() >= deadline) {
            return Err(DeliveryDeadline.into());
        }
    }
}

/// Only the explicit contract survives as 75. Generic errors, signals,
/// timeouts and uncertain acceptance remain ordinary fatal failures.
pub fn exit_code(error: &anyhow::Error) -> u8 {
    if error.is::<KnownNotSubmitted>() { KNOWN_NOT_SUBMITTED_EXIT } else { 1 }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn notice(attempt: u64) -> RetryNotice {
        RetryNotice { attempt, reason: "fixture known unsent", delay: Duration::ZERO }
    }

    #[test]
    fn retry_is_opt_in_and_context_preserves_the_typed_exit() {
        let error = RetryPolicy::new(false, None)
            .observed(Err(KnownNotSubmitted.into()))
            .unwrap_err().context("outer daemon report context");
        assert_eq!(exit_code(&error), 75);
        assert!(error.downcast_ref::<DeferredNotSubmitted>().is_none());
        assert_eq!(exit_code(&anyhow::anyhow!("generic timeout")), 1);
    }

    #[test]
    fn backoff_caps_and_success_resets_per_report_attempts() {
        let mut policy = RetryPolicy::new(true, None);
        for (index, seconds) in [5, 20, 60, 60, 60].into_iter().enumerate() {
            let error = policy.observed(Err(KnownNotSubmitted.into())).unwrap_err();
            let deferred = error.downcast_ref::<DeferredNotSubmitted>().unwrap();
            assert_eq!(deferred.notice.attempt, index as u64 + 1);
            assert_eq!(deferred.notice.delay, Duration::from_secs(seconds));
        }
        policy.observed(Ok(())).unwrap();
        let error = policy.observed(Err(KnownNotSubmitted.into())).unwrap_err();
        let deferred = error.downcast_ref::<DeferredNotSubmitted>().unwrap();
        assert_eq!(deferred.notice.attempt, 1);
        assert_eq!(deferred.notice.delay, Duration::from_secs(5));
    }

    #[test]
    fn generic_failure_never_becomes_a_deferred_retry() {
        let error = RetryPolicy::new(true, None)
            .observed(Err(anyhow::anyhow!("unknown dispatch"))).unwrap_err();
        assert!(error.downcast_ref::<DeferredNotSubmitted>().is_none());
    }

    #[test]
    fn deadline_clamps_callback_and_refuses_before_a_new_attempt() {
        let policy = RetryPolicy::new(true, Some(Instant::now()));
        assert!(policy.attempt_timeout(Duration::from_secs(30)).unwrap_err().is::<DeliveryDeadline>());
        let policy = RetryPolicy::new(true, Some(Instant::now() + Duration::from_secs(1)));
        assert!(policy.attempt_timeout(Duration::from_secs(30)).unwrap() <= Duration::from_secs(1));
    }

    #[test]
    fn retained_report_and_each_retry_notice_precede_acceptance() {
        crate::storage::runtime().unwrap().block_on(async {
            let mut attempts = Vec::new();
            let mut retries = Vec::new();
            let mut emit = |part| {
                let crate::out::Part::Text { text } = part else { panic!("text only") };
                attempts.push(text);
                if attempts.len() < 3 {
                    Err(DeferredNotSubmitted {
                        notice: notice(attempts.len() as u64),
                        wake_at: Instant::now(), deadline: None,
                    }.into())
                } else { Ok(()) }
            };
            complete_report(&mut crate::out::Out::new(&mut emit), "fixed π\n\n", "fixture",
                            &mut |notice| retries.push(notice)).await.unwrap();
            assert_eq!(attempts, vec!["fixed π\n\n"; 3]);
            assert_eq!(retries, vec![DeliveryNotice::Retry(notice(1)),
                DeliveryNotice::Retry(notice(2)), DeliveryNotice::Accepted]);
        });
    }

    #[test]
    fn deadline_during_backoff_never_acknowledges_or_reattempts() {
        crate::storage::runtime().unwrap().block_on(async {
            let mut attempts = 0;
            let mut retries = Vec::new();
            let mut emit = |_| {
                attempts += 1;
                Err(DeferredNotSubmitted {
                    notice: notice(attempts),
                    wake_at: Instant::now() + Duration::from_secs(60),
                    deadline: Some(Instant::now() + Duration::from_millis(5)),
                }.into())
            };
            let error = complete_report(&mut crate::out::Out::new(&mut emit), "fixed", "fixture",
                                        &mut |notice| retries.push(notice)).await.unwrap_err();
            assert!(error.is::<DeliveryDeadline>());
            assert_eq!(attempts, 1);
            assert_eq!(retries, vec![DeliveryNotice::Retry(notice(1))]);
        });
    }

    #[test]
    fn backoff_yields_to_the_daemon_stop_selector() {
        crate::storage::runtime().unwrap().block_on(async {
            let mut attempts = 0;
            let mut emit = |_| {
                attempts += 1;
                Err(DeferredNotSubmitted {
                    notice: notice(attempts),
                    wake_at: Instant::now() + Duration::from_secs(60),
                    deadline: None,
                }.into())
            };
            let mut output = crate::out::Out::new(&mut emit);
            tokio::select! {
                _ = tokio::time::sleep(Duration::from_millis(5)) => {},
                _ = complete_report(&mut output, "fixed", "fixture", &mut |_| {}) => panic!("must remain deferred"),
            }
            drop(output);
            drop(emit);
            assert_eq!(attempts, 1);
        });
    }
}
