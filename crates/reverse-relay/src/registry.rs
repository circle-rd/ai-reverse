//! Which nodes are connected, which models each serves, and which tunnel a request takes.

use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use hyper::client::conn::http2::SendRequest;
use reverse_core::body::ProxyBody;
use reverse_core::ids::{ClientId, ModelName};
use reverse_core::wire::ModelTag;
use tokio::sync::Notify;

/// A handle to send requests through one agent's tunnel.
pub type TunnelSender = SendRequest<ProxyBody>;

/// Last-seen time and tags of one `(client, model)` pair.
struct Sighting {
    last: Instant,
    tags: Vec<ModelTag>,
}

/// Last-seen times of `(client, model)` pairs.
///
/// A pair stays wanted for a grace period after its last sighting, which covers both an agent
/// reconnecting and an engine restarting between two lanes with one mechanism.
#[derive(Default)]
struct ModelBook {
    seen: BTreeMap<(ClientId, ModelName), Sighting>,
}

impl ModelBook {
    /// Records sightings; returns whether a pair appeared that was not wanted before.
    fn observe(
        &mut self,
        client: &ClientId,
        models: &[(ModelName, Vec<ModelTag>)],
        now: Instant,
    ) -> bool {
        let mut appeared = false;
        for (model, tags) in models {
            match self.seen.get_mut(&(client.clone(), model.clone())) {
                Some(sighting) => {
                    sighting.last = now;
                    sighting.tags.clone_from(tags);
                }
                None => {
                    self.seen.insert(
                        (client.clone(), model.clone()),
                        Sighting {
                            last: now,
                            tags: tags.clone(),
                        },
                    );
                    appeared = true;
                }
            }
        }
        appeared
    }

    /// Drops pairs older than `grace` and returns the rest with their latest tags.
    fn wanted(
        &mut self,
        grace: Duration,
        now: Instant,
    ) -> BTreeMap<(ClientId, ModelName), Vec<ModelTag>> {
        self.seen
            .retain(|_, sighting| now.saturating_duration_since(sighting.last) <= grace);
        self.seen
            .iter()
            .map(|(pair, sighting)| (pair.clone(), sighting.tags.clone()))
            .collect()
    }
}

/// Counts a request as in flight until dropped.
pub struct InflightGuard(Arc<AtomicUsize>);

impl InflightGuard {
    pub(crate) fn begin(counter: &Arc<AtomicUsize>) -> Self {
        counter.fetch_add(1, Ordering::Relaxed);
        Self(counter.clone())
    }
}

impl Drop for InflightGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::Relaxed);
    }
}

/// Everything needed to send one request through a client's tunnel.
pub struct Route {
    /// The tunnel.
    pub sender: TunnelSender,
    /// Public id of the node behind it.
    pub node_id: String,
    /// Keeps the node marked busy while alive.
    pub inflight: InflightGuard,
}

/// A connected node as the status listing sees it.
pub struct Live {
    /// Public id of the node.
    pub node_id: String,
    /// Requests being served right now.
    pub inflight: usize,
}

struct Tunnel {
    generation: u64,
    sender: TunnelSender,
    replaced: Arc<Notify>,
    node_id: String,
    inflight: Arc<AtomicUsize>,
    models: BTreeMap<ModelName, Vec<ModelTag>>,
    last_used: u64,
}

#[derive(Default)]
struct Inner {
    next_generation: u64,
    next_use: u64,
    tunnels: HashMap<ClientId, Vec<Tunnel>>,
    book: ModelBook,
}

/// Shared state of every tunnel, the API handlers and the reconciler.
#[derive(Default)]
pub struct Registry {
    inner: Mutex<Inner>,
    changed: Notify,
}

/// Keeps a tunnel registered; dropping it unregisters the tunnel unless it was replaced.
pub struct Lease {
    registry: Arc<Registry>,
    client: ClientId,
    node_id: String,
    generation: u64,
    replaced: Arc<Notify>,
}

impl Lease {
    /// Resolves when a newer tunnel for the same node took over, or the node was evicted.
    pub async fn replaced(&self) {
        self.replaced.notified().await;
    }
}

impl Drop for Lease {
    fn drop(&mut self) {
        let mut inner = self.registry.lock();
        if let Some(tunnels) = inner.tunnels.get_mut(&self.client) {
            tunnels.retain(|t| !(t.node_id == self.node_id && t.generation == self.generation));
            if tunnels.is_empty() {
                inner.tunnels.remove(&self.client);
            }
        }
    }
}

/// What the choice of a tunnel looks at: serves the model, requests in flight, last use.
type Candidate = (bool, usize, u64);

/// The index of the tunnel to use: among those serving the model (all of them when none is
/// known to), the least busy, then the one idle the longest.
fn pick(candidates: &[Candidate]) -> Option<usize> {
    let any_serves = candidates.iter().any(|(serves, ..)| *serves);
    candidates
        .iter()
        .enumerate()
        .filter(|(_, (serves, ..))| *serves || !any_serves)
        .min_by_key(|(_, (_, inflight, last_used))| (*inflight, *last_used))
        .map(|(index, _)| index)
}

impl Registry {
    /// An empty registry.
    #[must_use]
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().expect("registry lock poisoned")
    }

    /// Registers a tunnel; for one node the newest connection wins, so a half-open old one
    /// never blocks a reconnect, while the other nodes of the client stay connected.
    pub fn attach(
        self: &Arc<Self>,
        client: ClientId,
        node_id: String,
        sender: TunnelSender,
    ) -> Lease {
        let replaced = Arc::new(Notify::new());
        let mut inner = self.lock();
        inner.next_generation += 1;
        let generation = inner.next_generation;
        let tunnels = inner.tunnels.entry(client.clone()).or_default();
        let previous = tunnels
            .iter()
            .position(|t| t.node_id == node_id)
            .map(|index| tunnels.swap_remove(index));
        tunnels.push(Tunnel {
            generation,
            sender,
            replaced: replaced.clone(),
            node_id: node_id.clone(),
            inflight: Arc::new(AtomicUsize::new(0)),
            models: BTreeMap::new(),
            last_used: 0,
        });
        drop(inner);
        if let Some(old) = previous {
            tracing::info!(client_id = %client, node_id = %node_id, "tunnel.replaced");
            old.replaced.notify_one();
        }
        Lease {
            registry: self.clone(),
            client,
            node_id,
            generation,
            replaced,
        }
    }

    /// Closes the tunnel of a node, as when its credential is revoked.
    pub fn evict(&self, client: &ClientId, node_id: &str) {
        if let Some(tunnel) = self
            .lock()
            .tunnels
            .get(client)
            .and_then(|all| all.iter().find(|t| t.node_id == node_id))
        {
            tunnel.replaced.notify_one();
        }
    }

    /// Whether any node of the client is connected.
    #[must_use]
    pub fn connected(&self, client: &ClientId) -> bool {
        self.lock().tunnels.contains_key(client)
    }

    /// The route to one of the client's nodes, counting the request as in flight.
    ///
    /// A node serving `model` is preferred; before any node reported its models, or when none
    /// serves the model, every node is a candidate.
    #[must_use]
    pub fn route(&self, client: &ClientId, model: Option<&str>) -> Option<Route> {
        let mut inner = self.lock();
        inner.next_use += 1;
        let stamp = inner.next_use;
        let tunnels = inner.tunnels.get_mut(client)?;
        let candidates: Vec<Candidate> = tunnels
            .iter()
            .map(|t| {
                let serves = model.is_some_and(|m| t.models.keys().any(|x| x.as_str() == m));
                (serves, t.inflight.load(Ordering::Relaxed), t.last_used)
            })
            .collect();
        let tunnel = tunnels.get_mut(pick(&candidates)?)?;
        tunnel.last_used = stamp;
        Some(Route {
            sender: tunnel.sender.clone(),
            node_id: tunnel.node_id.clone(),
            inflight: InflightGuard::begin(&tunnel.inflight),
        })
    }

    /// The state of one connected node.
    #[must_use]
    pub fn live(&self, client: &ClientId, node_id: &str) -> Option<Live> {
        self.lock()
            .tunnels
            .get(client)?
            .iter()
            .find(|t| t.node_id == node_id)
            .map(|tunnel| Live {
                node_id: tunnel.node_id.clone(),
                inflight: tunnel.inflight.load(Ordering::Relaxed),
            })
    }

    /// Every connected `(client, node id)`.
    #[must_use]
    pub fn live_nodes(&self) -> Vec<(ClientId, String)> {
        self.lock()
            .tunnels
            .iter()
            .flat_map(|(client, all)| all.iter().map(|t| (client.clone(), t.node_id.clone())))
            .collect()
    }

    /// The models a connected node reported in its last poll, with their tags.
    #[must_use]
    pub fn node_models(&self, client: &ClientId, node_id: &str) -> Vec<(ModelName, Vec<ModelTag>)> {
        self.lock()
            .tunnels
            .get(client)
            .and_then(|all| all.iter().find(|t| t.node_id == node_id))
            .map(|t| {
                t.models
                    .iter()
                    .map(|(model, tags)| (model.clone(), tags.clone()))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Records the models a connected node serves right now.
    pub fn observe(&self, client: &ClientId, node_id: &str, models: &[(ModelName, Vec<ModelTag>)]) {
        let mut inner = self.lock();
        let Some(tunnel) = inner
            .tunnels
            .get_mut(client)
            .and_then(|all| all.iter_mut().find(|t| t.node_id == node_id))
        else {
            return;
        };
        tunnel.models = models.iter().cloned().collect();
        let appeared = inner.book.observe(client, models, Instant::now());
        drop(inner);
        if appeared {
            self.changed.notify_one();
        }
    }

    /// The pairs that must be registered in LiteLLM now, with their tags.
    #[must_use]
    pub fn wanted(&self, grace: Duration) -> BTreeMap<(ClientId, ModelName), Vec<ModelTag>> {
        self.lock().book.wanted(grace, Instant::now())
    }

    /// Resolves when a new model appeared.
    pub async fn changed(&self) {
        self.changed.notified().await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pair(client: &str, model: &str) -> (ClientId, ModelName) {
        (
            ClientId::parse(client).unwrap(),
            ModelName::parse(model).unwrap(),
        )
    }

    #[test]
    fn the_tunnel_serving_the_model_wins_then_the_least_busy_then_the_longest_idle() {
        // (serves, inflight, last_used)
        assert_eq!(pick(&[]), None);
        assert_eq!(pick(&[(false, 0, 1), (true, 5, 2)]), Some(1));
        assert_eq!(pick(&[(true, 3, 1), (true, 1, 2), (true, 2, 0)]), Some(1));
        assert_eq!(pick(&[(true, 1, 9), (true, 1, 4)]), Some(1));
    }

    #[test]
    fn every_tunnel_is_a_candidate_when_none_is_known_to_serve_the_model() {
        assert_eq!(pick(&[(false, 2, 1), (false, 0, 2)]), Some(1));
    }

    #[test]
    fn a_model_stays_wanted_through_the_grace_period_then_expires() {
        let mut book = ModelBook::default();
        let t0 = Instant::now();
        let (client, model) = pair("tester", "m1");
        let models = vec![(model.clone(), vec![ModelTag::Text])];
        assert!(book.observe(&client, &models, t0));
        let grace = Duration::from_secs(30);
        assert!(
            book.wanted(grace, t0 + Duration::from_secs(30))
                .contains_key(&(client.clone(), model.clone()))
        );
        assert!(book.wanted(grace, t0 + Duration::from_secs(31)).is_empty());
    }

    #[test]
    fn a_fresh_sighting_extends_the_grace_and_does_not_count_as_new() {
        let mut book = ModelBook::default();
        let t0 = Instant::now();
        let (client, model) = pair("tester", "m1");
        let models = vec![(model.clone(), vec![ModelTag::Text])];
        assert!(book.observe(&client, &models, t0));
        assert!(!book.observe(&client, &models, t0 + Duration::from_secs(20)));
        assert_eq!(
            book.wanted(Duration::from_secs(30), t0 + Duration::from_secs(45))
                .len(),
            1
        );
    }

    #[test]
    fn an_expired_model_that_returns_counts_as_new() {
        let mut book = ModelBook::default();
        let t0 = Instant::now();
        let (client, model) = pair("tester", "m1");
        let models = vec![(model.clone(), vec![ModelTag::Text])];
        book.observe(&client, &models, t0);
        assert!(
            book.wanted(Duration::from_secs(1), t0 + Duration::from_secs(5))
                .is_empty()
        );
        assert!(book.observe(&client, &models, t0 + Duration::from_secs(6)));
    }

    #[test]
    fn the_latest_tags_replace_earlier_ones() {
        let mut book = ModelBook::default();
        let t0 = Instant::now();
        let (client, model) = pair("tester", "m1");
        let first = vec![(model.clone(), vec![ModelTag::Text])];
        let second = vec![(model.clone(), vec![ModelTag::Text, ModelTag::Tools])];
        book.observe(&client, &first, t0);
        book.observe(&client, &second, t0);
        let wanted = book.wanted(Duration::from_secs(30), t0);
        assert_eq!(
            wanted.get(&(client, model)),
            Some(&vec![ModelTag::Text, ModelTag::Tools])
        );
    }
}
