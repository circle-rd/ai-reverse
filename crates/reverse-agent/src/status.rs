//! Samples the engine, the host and the GPUs on a timer and publishes one [`AgentStatus`].

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use reverse_core::shutdown::triggered;
use reverse_core::status::{AgentStatus, Benchmark, EngineStatus, ServingState, UNKNOWN};
use reverse_core::wire::{HardwareKind, ModelsSnapshot};
use tokio::sync::watch;
use tokio::time::{Instant, MissedTickBehavior, interval};

use crate::engine::{self, Memo, Reading};
use crate::gpu;
use crate::host::{self, CpuTimes};
use crate::upstream::Upstream;

/// Everything the sampler reads or is told.
pub struct Inputs {
    pub upstream: Arc<Upstream>,
    pub models: watch::Receiver<ModelsSnapshot>,
    pub benchmark: watch::Receiver<Benchmark>,
    pub inflight: Arc<AtomicU32>,
    pub hardware: HardwareKind,
    /// Capacity the operator declared (`AGENT_MAX_SLOTS`).
    pub max_slots: Option<i32>,
    pub every: Duration,
}

/// Publishes a fresh status every `inputs.every` until shutdown.
pub async fn run(
    inputs: Inputs,
    status: watch::Sender<AgentStatus>,
    mut shutdown: watch::Receiver<bool>,
) {
    let started = Instant::now();
    let mut ticker = interval(inputs.every);
    ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let mut memo = Memo::default();
    let mut cpu: Option<CpuTimes> = None;
    let mut was_available = false;
    loop {
        tokio::select! {
            _ = ticker.tick() => {}
            () = triggered(&mut shutdown) => return,
        }
        let available = inputs.models.borrow().engine_available;
        if was_available && !available {
            // The engine may come back as another server with other endpoints.
            memo.reset();
        }
        was_available = available;

        let reading = if available {
            engine::read(&inputs.upstream, &mut memo, inputs.max_slots).await
        } else {
            Reading::unavailable(inputs.max_slots)
        };
        let host = host::read_host(Path::new("/"), &mut cpu);
        let unified = inputs.hardware == HardwareKind::DgxSpark;
        let gpus = gpu::read(unified, host.memory_used_percent).await;
        let inflight = inputs.inflight.load(Ordering::Relaxed);

        // Without engine metrics the requests crossing this agent are the best load signal.
        let running = if reading.running == UNKNOWN {
            i32::try_from(inflight).unwrap_or(i32::MAX)
        } else {
            reading.running
        };
        let state = ServingState::from_load(available, running, reading.queued, reading.max_slots);
        let hint = if available { reading.hint() } else { None };
        let benchmark = inputs.benchmark.borrow().clone();
        status.send_replace(AgentStatus {
            agent_uptime_secs: started.elapsed().as_secs(),
            sampled_at: unix_now(),
            state,
            inflight_requests: inflight,
            engine: EngineStatus {
                kind: reading.kind,
                version: reading.version,
                available,
                metrics_enabled: reading.metrics_enabled,
                max_slots: reading.max_slots,
                running_requests: reading.running,
                queued_requests: reading.queued,
                tokens_per_second: reading.tokens_per_second,
                hint,
            },
            host,
            gpus,
            benchmark,
        });
    }
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// Counts the requests crossing the agent; the guard decrements on drop, even on cancellation.
pub struct InflightGuard(Arc<AtomicU32>);

impl InflightGuard {
    /// Starts counting one request.
    #[must_use]
    pub fn begin(counter: &Arc<AtomicU32>) -> Self {
        counter.fetch_add(1, Ordering::Relaxed);
        Self(Arc::clone(counter))
    }
}

impl Drop for InflightGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_guard_counts_only_while_alive() {
        let counter = Arc::new(AtomicU32::new(0));
        let first = InflightGuard::begin(&counter);
        let second = InflightGuard::begin(&counter);
        assert_eq!(counter.load(Ordering::Relaxed), 2);
        drop(first);
        assert_eq!(counter.load(Ordering::Relaxed), 1);
        drop(second);
        assert_eq!(counter.load(Ordering::Relaxed), 0);
    }
}
