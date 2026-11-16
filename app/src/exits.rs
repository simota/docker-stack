//! Tells a container that failed on its own from one that was stopped.
//!
//! The daemon sends `kill` before `die` whenever a container is stopped, restarted or
//! killed through its API (by this app, the CLI or compose). A `die` with a non-zero
//! exit code and no `kill` before it is a crash; an `oom` just before it is the kernel's
//! out-of-memory killer. A `health_status: unhealthy` is a container whose own health
//! check started failing while it still runs. Only the event stream is read, so
//! failures of containers this app did not start are seen too.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use engine::{EngineEvent, ResourceKind};

use crate::dto::{ContainerExit, ExitCause};

/// A `kill` older than this no longer explains a `die` (a stop waits 10 s by default).
const STOP_GRACE: Duration = Duration::from_secs(5 * 60);
/// An `oom` explains only a `die` this soon after it. The kernel can kill one process
/// in a container (a worker) while the container keeps running; when its main process
/// is the one killed, `die` follows within moments.
const OOM_WINDOW: Duration = Duration::from_secs(10);
/// At most one report per container in this time: a container that a restart policy
/// keeps bringing back would otherwise report every few seconds.
const QUIET: Duration = Duration::from_secs(60);

#[derive(Debug, Default)]
pub struct ExitWatch {
    /// Containers that were sent a signal through the API, and when.
    stopping: HashMap<String, Instant>,
    /// Containers the kernel reported out of memory, and when.
    oom: HashMap<String, Instant>,
    /// When each container was last reported, per kind of report (exit or unhealthy):
    /// a container that turns unhealthy and then crashes is reported both times.
    reported: HashMap<(String, bool), Instant>,
}

impl ExitWatch {
    /// Reads one event; returns the failure to report, if it is one.
    pub fn observe(&mut self, ev: &EngineEvent, now: Instant) -> Option<ContainerExit> {
        if ev.kind != ResourceKind::Container || ev.id.is_empty() {
            return None;
        }
        // `exec_die` and friends share the `die` prefix; only the exact action counts.
        match ev.action.as_str() {
            "kill" => {
                self.stopping
                    .retain(|_, at| now.duration_since(*at) < STOP_GRACE);
                self.stopping.insert(ev.id.clone(), now);
                None
            }
            "oom" => {
                self.oom
                    .retain(|_, at| now.duration_since(*at) < OOM_WINDOW);
                self.oom.insert(ev.id.clone(), now);
                None
            }
            "destroy" => {
                self.stopping.remove(&ev.id);
                self.oom.remove(&ev.id);
                self.reported.retain(|(id, _), _| *id != ev.id);
                None
            }
            "die" => {
                let stopped = self
                    .stopping
                    .remove(&ev.id)
                    .is_some_and(|at| now.duration_since(at) < STOP_GRACE);
                let oom = self
                    .oom
                    .remove(&ev.id)
                    .is_some_and(|at| now.duration_since(at) < OOM_WINDOW);
                let code = ev.exit_code.unwrap_or(0);
                if !(oom || (!stopped && code != 0)) {
                    return None;
                }
                let cause = if oom {
                    ExitCause::OutOfMemory(code)
                } else {
                    ExitCause::Code(code)
                };
                self.report(ev, cause, now)
            }
            // `health_status: healthy` / `: unhealthy` / `: starting`; sent when the
            // status changes.
            action if is_unhealthy(action) => self.report(ev, ExitCause::Unhealthy, now),
            _ => None,
        }
    }

    /// The report for `ev`, unless the container had one of its kind in the last
    /// [`QUIET`].
    fn report(
        &mut self,
        ev: &EngineEvent,
        cause: ExitCause,
        now: Instant,
    ) -> Option<ContainerExit> {
        let key = (ev.id.clone(), cause == ExitCause::Unhealthy);
        if self
            .reported
            .get(&key)
            .is_some_and(|at| now.duration_since(*at) < QUIET)
        {
            return None;
        }
        self.reported
            .retain(|_, at| now.duration_since(*at) < QUIET);
        self.reported.insert(key, now);
        Some(ContainerExit {
            id: ev.id.clone(),
            name: ev
                .name
                .clone()
                .unwrap_or_else(|| ev.id[..ev.id.len().min(12)].to_owned()),
            cause,
        })
    }
}

fn is_unhealthy(action: &str) -> bool {
    action
        .strip_prefix("health_status")
        .is_some_and(|rest| rest.trim_start_matches(':').trim() == "unhealthy")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(action: &str, id: &str, code: Option<i64>) -> EngineEvent {
        EngineEvent {
            kind: ResourceKind::Container,
            action: action.into(),
            id: id.into(),
            name: Some(format!("{id}-name")),
            exit_code: code,
        }
    }

    #[test]
    fn a_non_zero_exit_without_a_stop_is_reported() {
        let mut w = ExitWatch::default();
        let now = Instant::now();
        let exit = w.observe(&ev("die", "c1", Some(1)), now).unwrap();
        assert_eq!(
            exit,
            ContainerExit {
                id: "c1".into(),
                name: "c1-name".into(),
                cause: ExitCause::Code(1),
            }
        );
    }

    #[test]
    fn a_stop_kill_or_clean_exit_is_not_reported() {
        let mut w = ExitWatch::default();
        let now = Instant::now();
        // docker stop / restart / kill / compose down: `kill` comes first.
        assert_eq!(w.observe(&ev("kill", "c1", None), now), None);
        assert_eq!(w.observe(&ev("die", "c1", Some(137)), now), None);
        // A program that finished.
        assert_eq!(w.observe(&ev("die", "c2", Some(0)), now), None);
        // exec sessions are not the container.
        assert_eq!(w.observe(&ev("exec_die", "c3", Some(1)), now), None);
        // Other kinds of objects.
        let mut image = ev("die", "c4", Some(1));
        image.kind = ResourceKind::Image;
        assert_eq!(w.observe(&image, now), None);
    }

    #[test]
    fn a_kill_explains_only_the_next_die_and_only_for_a_while() {
        let mut w = ExitWatch::default();
        let t0 = Instant::now();
        w.observe(&ev("kill", "c1", None), t0);
        assert_eq!(w.observe(&ev("die", "c1", Some(143)), t0), None);
        // Started again later, then crashed on its own.
        assert!(w.observe(&ev("die", "c1", Some(2)), t0).is_some());
        // A kill long before the die does not hide it.
        w.observe(&ev("kill", "c2", None), t0);
        let later = t0 + STOP_GRACE + Duration::from_secs(1);
        assert!(w.observe(&ev("die", "c2", Some(1)), later).is_some());
    }

    #[test]
    fn out_of_memory_is_reported_even_after_a_kill() {
        let mut w = ExitWatch::default();
        let now = Instant::now();
        w.observe(&ev("oom", "c1", None), now);
        w.observe(&ev("kill", "c1", None), now);
        let exit = w.observe(&ev("die", "c1", Some(137)), now).unwrap();
        assert_eq!(exit.cause, ExitCause::OutOfMemory(137));
    }

    #[test]
    fn an_old_out_of_memory_kill_does_not_explain_a_later_stop() {
        let mut w = ExitWatch::default();
        let t0 = Instant::now();
        // One process in the container was killed; the container kept running.
        w.observe(&ev("oom", "c1", None), t0);
        // Much later the user stops it.
        let later = t0 + OOM_WINDOW + Duration::from_secs(1);
        w.observe(&ev("kill", "c1", None), later);
        assert_eq!(w.observe(&ev("die", "c1", Some(0)), later), None);
        // Nor does it turn a later crash into an out-of-memory report.
        w.observe(&ev("oom", "c2", None), t0);
        let exit = w.observe(&ev("die", "c2", Some(1)), later).unwrap();
        assert_eq!(exit.cause, ExitCause::Code(1));
    }

    #[test]
    fn a_restart_loop_is_reported_once_a_minute() {
        let mut w = ExitWatch::default();
        let t0 = Instant::now();
        assert!(w.observe(&ev("die", "c1", Some(1)), t0).is_some());
        let soon = t0 + Duration::from_secs(5);
        assert_eq!(w.observe(&ev("die", "c1", Some(1)), soon), None);
        // Another container is not held back.
        assert!(w.observe(&ev("die", "c2", Some(1)), soon).is_some());
        let later = t0 + QUIET + Duration::from_secs(1);
        assert!(w.observe(&ev("die", "c1", Some(1)), later).is_some());
        // Removing the container forgets it.
        w.observe(&ev("destroy", "c1", None), later);
        assert!(w.observe(&ev("die", "c1", Some(1)), later).is_some());
    }

    #[test]
    fn a_container_without_a_name_is_shown_by_its_short_id() {
        let mut w = ExitWatch::default();
        let mut e = ev("die", "0123456789abcdef", Some(3));
        e.name = None;
        let exit = w.observe(&e, Instant::now()).unwrap();
        assert_eq!(exit.name, "0123456789ab");
    }

    #[test]
    fn a_failing_health_check_is_reported_once_a_minute_and_apart_from_crashes() {
        let mut w = ExitWatch::default();
        let t0 = Instant::now();
        let unhealthy = w
            .observe(&ev("health_status: unhealthy", "c1", None), t0)
            .unwrap();
        assert_eq!(unhealthy.cause, ExitCause::Unhealthy);
        assert_eq!(unhealthy.name, "c1-name");
        // Healthy again, or still starting: nothing to say.
        assert_eq!(
            w.observe(&ev("health_status: healthy", "c1", None), t0),
            None
        );
        assert_eq!(
            w.observe(&ev("health_status: starting", "c1", None), t0),
            None
        );
        // Flapping is held back for a minute, but a crash right after is still told.
        let soon = t0 + Duration::from_secs(5);
        assert_eq!(
            w.observe(&ev("health_status: unhealthy", "c1", None), soon),
            None
        );
        assert!(w.observe(&ev("die", "c1", Some(1)), soon).is_some());
        let later = t0 + QUIET + Duration::from_secs(1);
        assert!(
            w.observe(&ev("health_status:unhealthy", "c1", None), later)
                .is_some()
        );
        // Removing the container forgets both.
        w.observe(&ev("destroy", "c1", None), later);
        assert!(
            w.observe(&ev("health_status: unhealthy", "c1", None), later)
                .is_some()
        );
        assert!(w.observe(&ev("die", "c1", Some(1)), later).is_some());
    }
}
