//! Background model-list probes for generic member-wide rests.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use hofund_plugin_api::{MemberId, PluginErrorKind};
use hofund_router::{Cooldowns, Member};
use tokio::time::Instant;

const STEPS: [Duration; 4] = [
    Duration::from_secs(60),
    Duration::from_secs(2 * 60),
    Duration::from_secs(5 * 60),
    Duration::from_secs(10 * 60),
];
const EXPIRY_MARGIN: Duration = Duration::from_secs(30);
const PROBE_TIMEOUT: Duration = Duration::from_secs(20);
const TICK: Duration = Duration::from_secs(1);

struct Schedule {
    until: Instant,
    next: Instant,
    step: usize,
    busy: bool,
}

type Answer = Result<
    Result<hofund_router::ModelListAnswer, hofund_plugin_api::PluginError>,
    tokio::time::error::Elapsed,
>;

/// Dropping the runner aborts every task still in its join set and drops its result.
pub(crate) struct ProbeRunner {
    states: HashMap<MemberId, Schedule>,
    tasks: tokio::task::JoinSet<(MemberId, Instant, Answer)>,
}

impl Default for ProbeRunner {
    fn default() -> Self {
        Self {
            states: HashMap::new(),
            tasks: tokio::task::JoinSet::new(),
        }
    }
}

impl ProbeRunner {
    pub(crate) async fn run_once(
        &mut self,
        cooldowns: &Arc<Cooldowns>,
        members: &[Arc<dyn Member>],
    ) {
        self.finish_ready(cooldowns);
        let now = Instant::now();
        let mut eligible = HashMap::new();
        for member in members {
            if !member.background_probe() || member.id().subscription().is_some() {
                continue;
            }
            let Some(cooling) = cooldowns.cooling(member.id()) else {
                continue;
            };
            if cooling.background_probe() && cooldowns.probe_allowed(member.id()) {
                eligible.insert(
                    member.id().clone(),
                    (member.clone(), Instant::from_std(cooling.until())),
                );
            }
        }
        self.states.retain(|id, state| {
            eligible
                .get(id)
                .is_some_and(|(_, until)| *until == state.until)
        });
        for (id, (_, until)) in &eligible {
            self.states.entry(id.clone()).or_insert_with(|| Schedule {
                until: *until,
                next: now + STEPS[0],
                step: 0,
                busy: false,
            });
        }

        let due = self
            .states
            .iter()
            .filter(|(_, state)| !state.busy && now >= state.next)
            .map(|(id, _)| id.clone())
            .collect::<Vec<_>>();
        for id in due {
            let Some(state) = self.states.get(&id) else {
                continue;
            };
            let until = state.until;
            if state.next + EXPIRY_MARGIN >= until {
                self.states.remove(&id);
                continue;
            }
            let Some((member, _)) = eligible.get(&id) else {
                continue;
            };
            self.states.get_mut(&id).expect("due schedule").busy = true;
            let member = Arc::clone(member);
            self.tasks.spawn(async move {
                let answer = tokio::time::timeout(PROBE_TIMEOUT, member.list_models()).await;
                (id, until, answer)
            });
        }
        tokio::task::yield_now().await;
        self.finish_ready(cooldowns);
    }

    fn finish_ready(&mut self, cooldowns: &Arc<Cooldowns>) {
        while let Some(Ok((id, until, answer))) = self.tasks.try_join_next() {
            if matches!(&answer, Ok(Ok(_))) && cooldowns.lift_probe(&id, until.into_std()) {
                self.states.remove(&id);
                continue;
            }
            let unsupported = matches!(
                answer,
                Ok(Err(ref error)) if error.kind() == PluginErrorKind::Unsupported
            );
            let Some(state) = self.states.get_mut(&id) else {
                continue;
            };
            if unsupported {
                self.states.remove(&id);
                continue;
            }
            state.busy = false;
            state.step = state.step.saturating_add(1);
            state.next = Instant::now() + STEPS[state.step.min(STEPS.len() - 1)];
            if state.next + EXPIRY_MARGIN >= state.until {
                self.states.remove(&id);
            }
        }
    }
}

pub(crate) fn spawn(
    cooldowns: Arc<Cooldowns>,
    members: impl Fn() -> Vec<Arc<dyn Member>> + Send + 'static,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut runner = ProbeRunner::default();
        let mut interval = tokio::time::interval(TICK);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            interval.tick().await;
            runner.run_once(&cooldowns, &members()).await;
        }
    })
}

#[cfg(test)]
mod tests;
