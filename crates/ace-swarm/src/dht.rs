//! Mainline DHT (BEP-5) `get_peers` — discover swarm peers for an infohash without a
//! tracker. Acestream populates the swarm on the public DHT, so an iterative `get_peers`
//! lookup toward the infohash returns the same peers the official engine connects to.
//!
//! This is a focused, time-bounded *lookup client* (not a full routing-table node): it
//! bootstraps off the well-known routers, walks closer to the target collecting `nodes`,
//! and harvests any `values` (peers) it's handed along the way.

use crate::resolver::Resolver;
use ace_wire::bencode::Bencode;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use std::time::{Duration, Instant};
use tokio::net::UdpSocket;

/// Well-known DHT bootstrap routers.
const BOOTSTRAP: &[&str] = &[
    "router.bittorrent.com:6881",
    "router.utorrent.com:6881",
    "dht.transmissionbt.com:6881",
    "dht.libtorrent.org:25401",
];
/// Enough peers to start racing TCP connects. Waiting for a much larger DHT harvest delays
/// first byte even though the engine only needs one good upstream.
const DHT_PEER_TARGET: usize = 8;

/// How long an inflight `(source, txid)` query stays eligible to be answered. Long enough to
/// admit a genuine reply that lands a round or two late, short enough that a stale entry can't
/// be matched by an unrelated packet arriving much later in the walk.
const INFLIGHT_TTL: Duration = Duration::from_secs(3);

/// How many `get_peers` queries the walk keeps outstanding at once.
const MAX_ACTIVE: usize = 8;

/// Bound per-walk address sets and transaction space even with a long budget and immediate
/// responses. Failed sends and recovery queries consume this same allowance.
const MAX_QUERY_ATTEMPTS: usize = 4096;

#[derive(Default)]
struct QueryBudget {
    attempts: usize,
}

impl QueryBudget {
    fn exhausted(&self) -> bool {
        self.attempts >= MAX_QUERY_ATTEMPTS
    }

    fn take(&mut self) -> Option<[u8; 2]> {
        if self.exhausted() {
            return None;
        }
        // MAX_QUERY_ATTEMPTS is below the two-byte transaction space. Never wrap/reuse an
        // id, and reserve it before attempting the send, including a failed OS send.
        let txid = u16::try_from(self.attempts).ok()?.to_be_bytes();
        self.attempts += 1;
        Some(txid)
    }
}

/// How long an unanswered query holds one of the [`MAX_ACTIVE`] slots before the walk sends
/// a replacement. Shorter than [`INFLIGHT_TTL`]: public DHT nodes are often silent, so a slot
/// waiting out the full TTL would halve the query rate on a sparse frontier. The query stays
/// correlatable until the TTL, so a late reply is still accepted.
const SLOT_TIMEOUT: Duration = Duration::from_millis(1500);

/// How many IPv4 addresses the walk seeds from one bootstrap hostname. Some routers sit
/// behind several addresses (round-robin DNS), and a walk that only ever asks the first
/// one loses that router whenever that single address drops its query.
const MAX_BOOTSTRAP_ADDRS_PER_HOST: usize = 2;

/// How many recently answering nodes the process remembers for the dead-end recovery round.
const NODE_CACHE_CAP: usize = 32;

/// How long a remembered node stays eligible. Public DHT nodes churn within hours; pymdht
/// refreshes its routing table every 15 minutes, so a node older than that is not trusted.
const NODE_CACHE_MAX_AGE: Duration = Duration::from_secs(15 * 60);

/// How many answering nodes one walk adds to the cache, and the most the recovery round
/// takes from it, so a single walk cannot crowd out what earlier walks learned.
const NODE_CACHE_PER_WALK: usize = 8;

/// Build a `get_peers` KRPC query for `infohash` from our `node_id`.
pub fn build_get_peers(node_id: &[u8; 20], infohash: &[u8; 20], txid: &[u8]) -> Vec<u8> {
    let mut a: BTreeMap<Vec<u8>, Bencode> = BTreeMap::new();
    a.insert(b"id".to_vec(), Bencode::Bytes(node_id.to_vec()));
    a.insert(b"info_hash".to_vec(), Bencode::Bytes(infohash.to_vec()));
    let mut d: BTreeMap<Vec<u8>, Bencode> = BTreeMap::new();
    d.insert(b"a".to_vec(), Bencode::Dict(a));
    d.insert(b"q".to_vec(), Bencode::Bytes(b"get_peers".to_vec()));
    d.insert(b"t".to_vec(), Bencode::Bytes(txid.to_vec()));
    d.insert(b"y".to_vec(), Bencode::Bytes(b"q".to_vec()));
    Bencode::Dict(d).encode()
}

/// Build a `find_node` KRPC query for `target` from our `node_id`. The walk only sends it
/// to a bootstrap router that already answered, during the dead-end recovery round: a
/// random target makes the router return nodes from another part of its table, which
/// gives a frontier whose first referrals were all silent somewhere new to start from.
pub fn build_find_node(node_id: &[u8; 20], target: &[u8; 20], txid: &[u8]) -> Vec<u8> {
    let mut a: BTreeMap<Vec<u8>, Bencode> = BTreeMap::new();
    a.insert(b"id".to_vec(), Bencode::Bytes(node_id.to_vec()));
    a.insert(b"target".to_vec(), Bencode::Bytes(target.to_vec()));
    let mut d: BTreeMap<Vec<u8>, Bencode> = BTreeMap::new();
    d.insert(b"a".to_vec(), Bencode::Dict(a));
    d.insert(b"q".to_vec(), Bencode::Bytes(b"find_node".to_vec()));
    d.insert(b"t".to_vec(), Bencode::Bytes(txid.to_vec()));
    d.insert(b"y".to_vec(), Bencode::Bytes(b"q".to_vec()));
    Bencode::Dict(d).encode()
}

/// Build an `announce_peer` KRPC query (BEP-5) — the DHT half of "make ourselves
/// discoverable as a peer for `infohash`". Must carry the opaque `token` a node handed us in
/// its own `get_peers` response (anti-spoofing: a node only accepts an announce echoing a
/// token it issued). `implied_port=0` and an explicit `port` — we advertise our real
/// listening port rather than relying on the sender's UDP source port.
pub fn build_announce_peer(
    node_id: &[u8; 20],
    infohash: &[u8; 20],
    port: u16,
    token: &[u8],
    txid: &[u8],
) -> Vec<u8> {
    let mut a: BTreeMap<Vec<u8>, Bencode> = BTreeMap::new();
    a.insert(b"id".to_vec(), Bencode::Bytes(node_id.to_vec()));
    a.insert(b"info_hash".to_vec(), Bencode::Bytes(infohash.to_vec()));
    a.insert(b"port".to_vec(), Bencode::Int(port as i64));
    a.insert(b"token".to_vec(), Bencode::Bytes(token.to_vec()));
    a.insert(b"implied_port".to_vec(), Bencode::Int(0));
    let mut d: BTreeMap<Vec<u8>, Bencode> = BTreeMap::new();
    d.insert(b"a".to_vec(), Bencode::Dict(a));
    d.insert(b"q".to_vec(), Bencode::Bytes(b"announce_peer".to_vec()));
    d.insert(b"t".to_vec(), Bencode::Bytes(txid.to_vec()));
    d.insert(b"y".to_vec(), Bencode::Bytes(b"q".to_vec()));
    Bencode::Dict(d).encode()
}

/// Parsed `get_peers` response: any peers handed to us, closer nodes to continue from, and
/// the opaque `token` (if present) needed to `announce_peer` back to this same node.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct GetPeersResponse {
    pub peers: Vec<SocketAddrV4>,
    pub nodes: Vec<([u8; 20], SocketAddrV4)>,
    pub token: Option<Vec<u8>>,
    /// The KRPC transaction id (`t`) this reply echoes, used to correlate it against the
    /// outbound query it answers. Empty if the response omitted `t`.
    pub txid: Vec<u8>,
}

/// Parse a KRPC response, extracting `r.values` (peers), `r.nodes` (compact nodes), and
/// `r.token` (needed to `announce_peer` back to whichever node sent this response).
pub fn parse_response(buf: &[u8]) -> Option<GetPeersResponse> {
    let v = Bencode::parse(buf).ok()?;
    // A KRPC *response* is the message type `y == "r"` (BEP-5). On untrusted public UDP a
    // hostile packet can smuggle an `r` dict under a query (`y == "q"`) or error
    // (`y == "e"`) type, or omit `y` entirely; gate on the type before mining `r` for
    // peers/nodes/tokens so those never come from a non-response.
    if v.get(b"y").and_then(Bencode::as_bytes) != Some(b"r".as_slice()) {
        return None;
    }
    // `r` must be a dict; a response whose `r` is any other shape is malformed, not empty.
    let r = v.get(b"r")?;
    if !matches!(r, Bencode::Dict(_)) {
        return None;
    }
    let mut out = GetPeersResponse::default();
    if let Some(Bencode::List(vals)) = r.get(b"values") {
        for e in vals {
            if let Some(b) = e.as_bytes() {
                if let Some(p) = compact_peer(b) {
                    out.peers.push(p);
                }
            }
        }
    }
    if let Some(nb) = r.get(b"nodes").and_then(Bencode::as_bytes) {
        for c in nb.as_chunks::<26>().0 {
            let mut id = [0u8; 20];
            id.copy_from_slice(&c[0..20]);
            if let Some(addr) = compact_peer(&c[20..26]) {
                out.nodes.push((id, addr));
            }
        }
    }
    out.token = r
        .get(b"token")
        .and_then(Bencode::as_bytes)
        .map(|b| b.to_vec());
    // Transaction id lives at the top level (`t`), alongside `r`/`y` — capture it so the walk
    // can correlate this reply against the query it answers.
    out.txid = v
        .get(b"t")
        .and_then(Bencode::as_bytes)
        .map(|b| b.to_vec())
        .unwrap_or_default();
    Some(out)
}

/// The transaction id of a rejected datagram that is a well-formed KRPC error (`y == "e"`),
/// or `None` if it is unparseable or the wrong shape. Separates a node that answered with an
/// error from genuinely malformed/hostile traffic, and lets a correlated error close its
/// query. An error without a usable `t` yields an empty id, which never matches a query.
/// BEP-5 requires `e` to be `[code, message]`; an error type without that payload is
/// malformed and must not close a query.
fn krpc_error_txid(buf: &[u8]) -> Option<Vec<u8>> {
    let v = Bencode::parse(buf).ok()?;
    if v.get(b"y").and_then(Bencode::as_bytes) != Some(b"e".as_slice()) {
        return None;
    }
    match v.get(b"e") {
        Some(Bencode::List(e)) if matches!(e.as_slice(), [Bencode::Int(_), Bencode::Bytes(_)]) => {}
        _ => return None,
    }
    Some(
        v.get(b"t")
            .and_then(Bencode::as_bytes)
            .map(<[u8]>::to_vec)
            .unwrap_or_default(),
    )
}

/// Per-walk diagnostics for a single iterative lookup. A raw peer count hides the failure
/// modes that matter when comparing the custom DHT against a candidate crate (#43): silence,
/// malformed traffic, KRPC errors, uncorrelated packets, and frontier exhaustion. Logged once
/// at the end of a walk and returned so tests can assert on it; not part of the public API.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct DhtWalkMetrics {
    /// Bootstrap/seed nodes placed in the initial frontier.
    bootstrap_seeded: usize,
    /// All attempted sends, successful or not, including recovery traffic.
    query_attempts: usize,
    /// Lookup queries sent, including the one bounded recovery round.
    nodes_queried: usize,
    /// Datagrams accepted as correlated `get_peers` responses.
    valid_responses: usize,
    /// Peer records harvested from accepted responses (`r.values`).
    peers_discovered: usize,
    /// Compact node records harvested from accepted responses (`r.nodes`).
    nodes_discovered: usize,
    /// Datagrams that failed to parse as a well-formed `get_peers` response and were not
    /// recognizable KRPC errors.
    malformed_responses: usize,
    /// Datagrams that were well-formed KRPC error messages (`y == "e"`).
    krpc_errors: usize,
    /// Datagrams that parsed as a response but matched no inflight `(source, txid)` query —
    /// late, replayed, or spoofed traffic rejected by correlation (#40).
    uncorrelated_responses: usize,
    /// Queries whose slot timeout elapsed without any datagram at all from that node.
    timeouts: usize,
    /// Queries sent by the one dead-end recovery round (#494), which runs only when the
    /// frontier runs dry before any peer was found: a fresh `get_peers` to each bootstrap
    /// router that never answered, and a random-target `find_node` to each that did.
    recovery_queries: usize,
    /// Peer records harvested after the recovery round started.
    recovery_peers: usize,
    /// The walk ran out of new frontier nodes before exhausting its time budget.
    frontier_exhausted: bool,
}

/// 6-byte compact endpoint: 4-byte IPv4 + 2-byte big-endian port.
fn compact_peer(b: &[u8]) -> Option<SocketAddrV4> {
    if b.len() != 6 {
        return None;
    }
    let port = u16::from_be_bytes([b[4], b[5]]);
    if port == 0 {
        return None;
    }
    Some(SocketAddrV4::new(
        Ipv4Addr::new(b[0], b[1], b[2], b[3]),
        port,
    ))
}

/// XOR distance between a node id and the target (BTreeSet-orderable key).
fn distance(id: &[u8; 20], target: &[u8; 20]) -> [u8; 20] {
    let mut d = [0u8; 20];
    for i in 0..20 {
        d[i] = id[i] ^ target[i];
    }
    d
}

/// Iterative `get_peers` walk toward `infohash`, shared by `dht_get_peers` (harvest peers)
/// and `dht_announce_peer` (harvest tokens to announce back with). Seeds the frontier from
/// bootstrap routers, keeps queries outstanding to the closest not-yet-queried nodes, and calls
/// `on_response` for each reply — which returns `true` to stop the walk early (e.g. "enough
/// peers" or "enough tokens"). Bounded by `budget` regardless.
async fn dht_walk(
    infohash: &[u8; 20],
    budget: Duration,
    sock: &UdpSocket,
    on_response: impl FnMut(SocketAddrV4, &GetPeersResponse) -> bool,
) -> DhtWalkMetrics {
    dht_walk_with_bootstraps(
        infohash,
        budget,
        sock,
        BOOTSTRAP,
        Resolver::global(),
        Some(NodeCache::global()),
        on_response,
    )
    .await
}

async fn dht_walk_with_bootstraps(
    infohash: &[u8; 20],
    budget: Duration,
    sock: &UdpSocket,
    bootstraps: &[&str],
    resolver: Resolver,
    cache: Option<&NodeCache>,
    on_response: impl FnMut(SocketAddrV4, &GetPeersResponse) -> bool,
) -> DhtWalkMetrics {
    let deadline = tokio::time::Instant::now() + budget;
    let mut lookups = tokio::task::JoinSet::new();
    for (i, host) in bootstraps.iter().take(BOOTSTRAP.len()).enumerate() {
        let resolver = resolver.clone();
        let host = host.to_string();
        lookups.spawn(async move { (i, resolve_bootstrap(host, resolver, deadline).await) });
    }
    dht_walk_owned(
        infohash,
        deadline.into_std(),
        sock,
        BTreeMap::new(),
        Vec::new(),
        lookups,
        cache,
        on_response,
    )
    .await
}

/// Resolve one bootstrap hostname to at most [`MAX_BOOTSTRAP_ADDRS_PER_HOST`] distinct IPv4
/// addresses, in resolver order. Empty if the lookup fails or misses `deadline`.
pub(crate) async fn resolve_bootstrap(
    host: String,
    resolver: Resolver,
    deadline: tokio::time::Instant,
) -> Vec<SocketAddrV4> {
    let Ok(Ok(addresses)) = tokio::time::timeout_at(deadline, resolver.lookup(&host)).await else {
        return Vec::new();
    };
    let mut out: Vec<SocketAddrV4> = Vec::with_capacity(MAX_BOOTSTRAP_ADDRS_PER_HOST);
    for addr in addresses {
        if let SocketAddr::V4(v4) = addr {
            if !out.contains(&v4) {
                out.push(v4);
                if out.len() == MAX_BOOTSTRAP_ADDRS_PER_HOST {
                    break;
                }
            }
        }
    }
    out
}

#[cfg(test)]
async fn dht_walk_from_seeds(
    infohash: &[u8; 20],
    budget: Duration,
    sock: &UdpSocket,
    seeds: Vec<SocketAddrV4>,
    on_response: impl FnMut(SocketAddrV4, &GetPeersResponse) -> bool,
) -> DhtWalkMetrics {
    let mut frontier: BTreeMap<[u8; 20], SocketAddrV4> = BTreeMap::new();
    for (i, seed) in seeds.into_iter().enumerate() {
        let mut key = [0xffu8; 20];
        key[19] = i as u8;
        frontier.insert(key, seed);
    }
    dht_walk_frontier(infohash, budget, sock, frontier, on_response).await
}

/// Like [`dht_walk_from_seeds`], but the seeds stand in for resolved bootstrap routers, so
/// they are eligible for the dead-end recovery round.
#[cfg(test)]
async fn dht_walk_from_bootstrap_seeds(
    infohash: &[u8; 20],
    budget: Duration,
    sock: &UdpSocket,
    seeds: Vec<SocketAddrV4>,
    cache: Option<&NodeCache>,
    on_response: impl FnMut(SocketAddrV4, &GetPeersResponse) -> bool,
) -> DhtWalkMetrics {
    let mut frontier: BTreeMap<[u8; 20], SocketAddrV4> = BTreeMap::new();
    let mut bootstraps = Vec::new();
    for (i, seed) in seeds.into_iter().enumerate() {
        let mut key = [0xffu8; 20];
        key[19] = i as u8;
        frontier.insert(key, seed);
        bootstraps.push(BootstrapNode {
            addr: seed,
            answered: false,
        });
    }
    dht_walk_owned(
        infohash,
        Instant::now() + budget,
        sock,
        frontier,
        bootstraps,
        tokio::task::JoinSet::new(),
        cache,
        on_response,
    )
    .await
}

#[cfg(test)]
async fn dht_walk_frontier(
    infohash: &[u8; 20],
    budget: Duration,
    sock: &UdpSocket,
    frontier: BTreeMap<[u8; 20], SocketAddrV4>,
    on_response: impl FnMut(SocketAddrV4, &GetPeersResponse) -> bool,
) -> DhtWalkMetrics {
    dht_walk_owned(
        infohash,
        Instant::now() + budget,
        sock,
        frontier,
        Vec::new(),
        tokio::task::JoinSet::new(),
        None,
        on_response,
    )
    .await
}

/// A resolved bootstrap router address. Kept apart from the frontier, whose 64-entry cap
/// evicts the far-away bootstrap keys first, so the recovery round can still reach it.
struct BootstrapNode {
    addr: SocketAddrV4,
    /// Whether a correlated, well-formed response came back from this address.
    answered: bool,
}

type BootstrapLookups = tokio::task::JoinSet<(usize, Vec<SocketAddrV4>)>;

fn add_bootstrap(
    result: Result<(usize, Vec<SocketAddrV4>), tokio::task::JoinError>,
    frontier: &mut BTreeMap<[u8; 20], SocketAddrV4>,
    bootstraps: &mut Vec<BootstrapNode>,
    responded: &HashSet<SocketAddrV4>,
    metrics: &mut DhtWalkMetrics,
) {
    let Ok((i, addrs)) = result else {
        return;
    };
    for (j, addr) in addrs.into_iter().enumerate() {
        if bootstraps.iter().any(|node| node.addr == addr) {
            continue;
        }
        // One key per (host, address), still in the farthest-possible key range.
        let mut key = [0xff; 20];
        key[18] = i as u8;
        key[19] = j as u8;
        frontier.insert(key, addr);
        // A router can be referred to, asked and answer before its own lookup finishes;
        // it still counts as answered, so recovery widens it with `find_node`.
        bootstraps.push(BootstrapNode {
            addr,
            answered: responded.contains(&addr),
        });
        metrics.bootstrap_seeded += 1;
    }
    while frontier.len() > 64 {
        let key = *frontier.keys().next_back().unwrap();
        frontier.remove(&key);
    }
}
/// One outstanding query. A node has at most one live query at a time, so the destination
/// alone identifies it; the transaction id is checked on every reply. Only the recovery
/// round asks a node twice, and its fresh query replaces the released first one.
struct Inflight {
    txid: [u8; 2],
    /// When the query stops holding one of the [`MAX_ACTIVE`] slots. A reply after this is
    /// still accepted until `expires`; it just no longer blocks a replacement query.
    release: Instant,
    /// When the query is abandoned and its `(source, txid)` can no longer be matched.
    expires: Instant,
    /// Whether the query still holds a slot (no correlated reply, `release` not reached).
    active: bool,
    /// Whether any datagram at all arrived from this node.
    heard: bool,
}

/// Nodes that recently gave a correlated, well-formed reply (#522), newest first. Only the
/// dead-end recovery round reads it, next to the bootstrap routers, so a walk that never
/// dead-ends is not slowed by stale entries (the startup cost that ruled out #42). Bounded
/// at [`NODE_CACHE_CAP`] entries and kept in memory only.
pub(crate) struct NodeCache {
    nodes: std::sync::Mutex<VecDeque<(SocketAddrV4, Instant)>>,
}

impl NodeCache {
    pub(crate) const fn new() -> Self {
        Self {
            nodes: std::sync::Mutex::new(VecDeque::new()),
        }
    }

    /// The process-wide cache the real walks share.
    fn global() -> &'static Self {
        static CACHE: NodeCache = NodeCache::new();
        &CACHE
    }

    /// Remember each `(address, answered at)` pair, keeping the time the node actually
    /// answered so its age is never understated. The cache stays ordered by reply time,
    /// newest first; among equal times the first entry goes in front.
    fn record_answered(&self, entries: &[(SocketAddrV4, Instant)]) {
        let mut nodes = self.nodes.lock().unwrap_or_else(|e| e.into_inner());
        for (addr, seen) in entries.iter().rev() {
            // Overlapping walks can report the same node out of order. A strictly older reply
            // changes nothing: the entry keeps its newer time and its place.
            if let Some(pos) = nodes.iter().position(|(known, _)| known == addr) {
                if nodes[pos].1 > *seen {
                    continue;
                }
                nodes.remove(pos);
            }
            // Keep the whole cache ordered by reply time, newest first, whichever walk
            // finishes first: ahead of every entry no newer than this reply.
            let at = nodes
                .iter()
                .position(|(_, other)| *other <= *seen)
                .unwrap_or(nodes.len());
            nodes.insert(at, (*addr, *seen));
        }
        nodes.truncate(NODE_CACHE_CAP);
    }

    /// Remember `addrs` as all answering at `now`, the first one at the front.
    #[cfg(test)]
    fn record(&self, addrs: &[SocketAddrV4], now: Instant) {
        let entries: Vec<(SocketAddrV4, Instant)> = addrs.iter().map(|a| (*a, now)).collect();
        self.record_answered(&entries);
    }

    /// Up to `limit` remembered nodes still younger than [`NODE_CACHE_MAX_AGE`] at `now`,
    /// newest first, skipping any `skip` rejects. Expired entries are dropped.
    fn fresh(
        &self,
        now: Instant,
        limit: usize,
        skip: impl Fn(&SocketAddrV4) -> bool,
    ) -> Vec<SocketAddrV4> {
        let mut nodes = self.nodes.lock().unwrap_or_else(|e| e.into_inner());
        nodes.retain(|(_, seen)| now.saturating_duration_since(*seen) < NODE_CACHE_MAX_AGE);
        nodes
            .iter()
            .map(|(addr, _)| *addr)
            .filter(|addr| !skip(addr))
            .take(limit)
            .collect()
    }
}

/// The answering nodes one walk hands to the [`NodeCache`]: those that returned peers first,
/// as they sit in a populated part of the DHT, then the rest. Bootstrap routers are left out,
/// since the recovery round asks them anyway.
/// How many answering nodes of each kind one walk keeps as cache candidates: the per-walk
/// limit plus every bootstrap address a walk can seed.
const ANSWERED_CANDIDATES: usize =
    NODE_CACHE_PER_WALK + BOOTSTRAP.len() * MAX_BOOTSTRAP_ADDRS_PER_HOST;

/// Each node keeps the time its reply arrived, not the time the walk ended.
#[derive(Default)]
struct Answered {
    with_peers: Vec<(SocketAddrV4, Instant)>,
    without_peers: Vec<(SocketAddrV4, Instant)>,
}

impl Answered {
    fn note(&mut self, addr: SocketAddrV4, had_peers: bool, at: Instant) {
        let list = if had_peers {
            &mut self.with_peers
        } else {
            &mut self.without_peers
        };
        // Room for every bootstrap address on top of the per-walk limit: a router can be
        // noted here before its own lookup finishes, and is filtered out only at the end.
        if list.len() < ANSWERED_CANDIDATES && !list.iter().any(|(known, _)| *known == addr) {
            list.push((addr, at));
        }
    }

    /// Up to [`NODE_CACHE_PER_WALK`] candidates, those with peers first, after dropping any
    /// `skip` rejects, so a filtered-out router never costs a real responder its place.
    fn into_preferred_first(
        self,
        skip: impl Fn(&SocketAddrV4) -> bool,
    ) -> Vec<(SocketAddrV4, Instant)> {
        let mut out: Vec<(SocketAddrV4, Instant)> = Vec::with_capacity(NODE_CACHE_PER_WALK);
        for (addr, at) in self.with_peers.into_iter().chain(self.without_peers) {
            if out.len() == NODE_CACHE_PER_WALK {
                break;
            }
            if !skip(&addr) && !out.iter().any(|(known, _)| *known == addr) {
                out.push((addr, at));
            }
        }
        out
    }
}

/// Which addresses the recovery round asks, at most [`MAX_ACTIVE`]. Every bootstrap router
/// is asked exactly as in the #494 round, in the same order; remembered nodes only take
/// the slots the routers leave free, so a stale entry can never displace a router.
fn recovery_targets(
    bootstraps: &[BootstrapNode],
    cached: Vec<SocketAddrV4>,
) -> Vec<(SocketAddrV4, RecoveryQuery)> {
    let mut out: Vec<(SocketAddrV4, RecoveryQuery)> = bootstraps
        .iter()
        .take(MAX_ACTIVE)
        .map(|node| {
            let query = if node.answered {
                RecoveryQuery::FindNode
            } else {
                RecoveryQuery::GetPeers
            };
            (node.addr, query)
        })
        .collect();
    for addr in cached {
        if out.len() == MAX_ACTIVE {
            break;
        }
        if !out.iter().any(|(known, _)| *known == addr) {
            out.push((addr, RecoveryQuery::GetPeers));
        }
    }
    out
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RecoveryQuery {
    GetPeers,
    FindNode,
}

#[allow(clippy::too_many_arguments)]
async fn dht_walk_owned(
    infohash: &[u8; 20],
    deadline: Instant,
    sock: &UdpSocket,
    mut frontier: BTreeMap<[u8; 20], SocketAddrV4>,
    mut bootstraps: Vec<BootstrapNode>,
    mut lookups: BootstrapLookups,
    cache: Option<&NodeCache>,
    mut on_response: impl FnMut(SocketAddrV4, &GetPeersResponse) -> bool,
) -> DhtWalkMetrics {
    let node_id: [u8; 20] = rand::random();
    let mut answered = Answered::default();
    let mut metrics = DhtWalkMetrics {
        bootstrap_seeded: frontier.len(),
        ..Default::default()
    };
    // Peers harvested when the recovery round started; `None` until it runs. One round per
    // walk bounds its extra traffic to one datagram per bootstrap address.
    let mut recovery_started_at: Option<usize> = None;

    let mut queried: HashSet<SocketAddrV4> = HashSet::new();
    // Every source of a correlated, well-formed reply, bounded by the queries sent.
    let mut responded: HashSet<SocketAddrV4> = HashSet::new();
    // Public DHT UDP is untrusted, so a reply is only trusted if it arrives from the exact node
    // we asked and echoes the exact transaction id we generated for that query.
    let mut inflight: HashMap<SocketAddrV4, Inflight> = HashMap::new();
    let mut query_budget = QueryBudget::default();
    let mut buf = [0u8; 2048];
    let tokio_deadline = tokio::time::Instant::from_std(deadline);
    crate::alog!("[dht] seeded {} bootstrap node(s)", frontier.len());

    // Paced by responses (#384): keep up to MAX_ACTIVE queries outstanding, sending the next
    // closest node as soon as a correlated reply or a slot timeout frees a slot, instead of
    // waiting out a fixed window per batch. The absolute lookup deadline is unchanged.
    'walk: while Instant::now() < deadline {
        while let Some(result) = lookups.try_join_next() {
            add_bootstrap(
                result,
                &mut frontier,
                &mut bootstraps,
                &responded,
                &mut metrics,
            );
        }
        let now = Instant::now();
        for query in inflight.values_mut() {
            if query.active && query.release <= now {
                query.active = false;
                if !query.heard {
                    metrics.timeouts += 1;
                }
            }
        }
        // Drop expired entries so a stale (source, txid) can't be matched by a much later
        // packet: an unanswered query is abandoned, not left open for the whole walk.
        inflight.retain(|_, query| query.expires > now);

        let mut active = inflight.values().filter(|query| query.active).count();
        while active < MAX_ACTIVE && !query_budget.exhausted() && Instant::now() < deadline {
            let Some(addr) = frontier
                .values()
                .find(|addr| !queried.contains(*addr))
                .copied()
            else {
                break;
            };
            queried.insert(addr);
            let Some(txid) = query_budget.take() else {
                break;
            };
            let query = build_get_peers(&node_id, infohash, &txid);
            let sent =
                tokio::time::timeout_at(tokio_deadline, sock.send_to(&query, SocketAddr::V4(addr)))
                    .await
                    .is_ok_and(|sent| sent.is_ok());
            if sent {
                metrics.nodes_queried += 1;
                let sent_at = Instant::now();
                inflight.insert(
                    addr,
                    Inflight {
                        txid,
                        release: sent_at + SLOT_TIMEOUT,
                        expires: sent_at + INFLIGHT_TTL,
                        active: true,
                        heard: false,
                    },
                );
                active += 1;
            }
        }

        if active == 0
            && lookups.is_empty()
            && recovery_started_at.is_none()
            && metrics.peers_discovered == 0
            && !query_budget.exhausted()
        {
            // The frontier ran dry inside the budget without a single peer, usually because
            // the routers' first referrals were all silent (#494). Once per walk, ask every
            // bootstrap address again: a fresh `get_peers` where the first query got no
            // answer, and a random-target `find_node` where it did, for a different set of
            // nodes. A walk that already holds peers ends as before: on a sparse swarm the
            // round found nothing new and only made the walk longer. `peers_discovered` only
            // counts correlated replies, so spoofed traffic can neither start nor suppress
            // the round. `dht_announce_peer` walks follow the same rule; they run in the
            // background, so the extra round costs them nothing visible.
            //
            // Nodes that answered earlier walks (#522) join the round, so a network where
            // the routers are slow or unreachable still has somewhere live to restart from.
            // Nodes this walk already asked are skipped: their answer led here.
            recovery_started_at = Some(metrics.peers_discovered);
            let queried_before = queried.len();
            let cached = cache.map_or_else(Vec::new, |cache| {
                cache.fresh(Instant::now(), NODE_CACHE_PER_WALK, |addr| {
                    queried.contains(addr) || bootstraps.iter().any(|node| node.addr == *addr)
                })
            });
            for (addr, kind) in recovery_targets(&bootstraps, cached) {
                if Instant::now() >= deadline {
                    break;
                }
                let Some(txid) = query_budget.take() else {
                    break;
                };
                let query = match kind {
                    RecoveryQuery::FindNode => build_find_node(&node_id, &rand::random(), &txid),
                    RecoveryQuery::GetPeers => build_get_peers(&node_id, infohash, &txid),
                };
                let sent = tokio::time::timeout_at(
                    tokio_deadline,
                    sock.send_to(&query, SocketAddr::V4(addr)),
                )
                .await
                .is_ok_and(|sent| sent.is_ok());
                if sent {
                    metrics.nodes_queried += 1;
                    metrics.recovery_queries += 1;
                    queried.insert(addr);
                    let sent_at = Instant::now();
                    // Replaces the first query's entry, whose slot was already released: a
                    // late reply to the old txid is now uncorrelated, and only the fresh txid
                    // from this exact address is accepted.
                    inflight.insert(
                        addr,
                        Inflight {
                            txid,
                            release: sent_at + SLOT_TIMEOUT,
                            expires: sent_at + INFLIGHT_TTL,
                            active: true,
                            heard: false,
                        },
                    );
                    active += 1;
                }
            }
            if active > 0 {
                crate::alog!(
                    "[dht] frontier dry with no peers after queried={}: recovery round sent {}",
                    queried_before,
                    metrics.recovery_queries
                );
            }
        }
        if active == 0 && query_budget.exhausted() {
            // In-flight work kept its normal slot clock after the final permitted send.
            // DNS cannot lead to another query now, so do not wait for it or busy-loop.
            break;
        }
        if active == 0 && lookups.is_empty() {
            // No active work remains. The TTL only admits late replies while another query
            // or DNS lookup keeps this walk alive; it never adds an idle-only wait.
            metrics.frontier_exhausted = true;
            crate::alog!("[dht] frontier exhausted: queried={}", queried.len());
            break;
        }
        // Wake for the next slot release so a silent node can't hold its slot past it.
        let wake = inflight
            .values()
            .filter(|query| query.active)
            .map(|query| query.release)
            .min()
            .map_or(deadline, |release| release.min(deadline));

        let received = tokio::select! {
            result = lookups.join_next(), if !lookups.is_empty() => {
                if let Some(result) = result {
                    add_bootstrap(
                result,
                &mut frontier,
                &mut bootstraps,
                &responded,
                &mut metrics,
            );
                }
                continue;
            }
            received = sock.recv_from(&mut buf) => received,
            _ = tokio::time::sleep_until(tokio::time::Instant::from_std(wake)) => continue,
        };
        if Instant::now() >= deadline {
            break;
        }
        // A receive error (e.g. an ICMP reset some platforms surface on unconnected UDP)
        // consumes one event and the walk carries on.
        let Ok((n, SocketAddr::V4(src))) = received else {
            continue;
        };
        let Some(resp) = parse_response(&buf[..n]) else {
            // Rejected before correlation: either a node's KRPC error or genuinely
            // malformed/hostile traffic. Bucket the two apart for diagnostics.
            let error_txid = krpc_error_txid(&buf[..n]);
            if let Some(query) = inflight.get_mut(&src) {
                query.heard = true;
                // A KRPC error that echoes this query's transaction is the node's final word:
                // close the query so it no longer holds a slot. Anything else from this source
                // (malformed, or an error for another transaction) could be spoofed, so the
                // query stays open for a genuine reply.
                if error_txid.as_deref() == Some(query.txid.as_slice())
                    && query.expires > Instant::now()
                {
                    inflight.remove(&src);
                }
            }
            if error_txid.is_some() {
                metrics.krpc_errors += 1;
            } else {
                metrics.malformed_responses += 1;
            }
            continue;
        };
        // Correlate: only accept a reply whose (source, txid) matches an outstanding query.
        // Removing the entry also means a duplicate/replayed packet for the same query is not
        // processed twice. Unmatched packets never free a slot, so spoofed traffic can't
        // speed the walk up or cut it short.
        // The expiry is re-checked here because `retain` only runs when the loop wakes, and a
        // packet can land after `expires` but before the next wake.
        match inflight.get_mut(&src) {
            Some(query) if resp.txid == query.txid && query.expires > Instant::now() => {
                inflight.remove(&src);
            }
            Some(query) => {
                query.heard = true;
                metrics.uncorrelated_responses += 1;
                continue;
            }
            None => {
                metrics.uncorrelated_responses += 1;
                continue;
            }
        }
        metrics.valid_responses += 1;
        responded.insert(src);
        if let Some(node) = bootstraps.iter_mut().find(|node| node.addr == src) {
            node.answered = true;
        } else {
            answered.note(src, !resp.peers.is_empty(), Instant::now());
        }
        metrics.peers_discovered += resp.peers.len();
        metrics.nodes_discovered += resp.nodes.len();
        for (id, addr) in &resp.nodes {
            frontier.entry(distance(id, infohash)).or_insert(*addr);
        }
        // Cap frontier growth: keep the 64 closest.
        while frontier.len() > 64 {
            let last = *frontier.keys().next_back().unwrap();
            frontier.remove(&last);
        }
        if on_response(src, &resp) {
            break 'walk;
        }
    }

    metrics.query_attempts = query_budget.attempts;
    if let Some(before) = recovery_started_at {
        metrics.recovery_peers = metrics.peers_discovered - before;
    }
    // An early successful stop can leave a router's DNS unresolved after that same router
    // answered as a referral. Skip best-effort learning in that case rather than cache a
    // still-unidentified bootstrap, or delay peer delivery just to finish its lookup.
    if let Some(cache) = cache.filter(|_| lookups.is_empty()) {
        // A router can answer as a referral before its own lookup finishes and be noted
        // here; the recovery round asks routers anyway, so it takes no cache slot.
        let entries =
            answered.into_preferred_first(|addr| bootstraps.iter().any(|node| node.addr == *addr));
        cache.record_answered(&entries);
    }
    crate::alog!(
        "[dht] walk done: seeded={} attempts={} queried={} valid={} peers={} nodes={} malformed={} \
         krpc_err={} uncorrelated={} timeouts={} recovery_queries={} recovery_peers={} \
         frontier_exhausted={}",
        metrics.bootstrap_seeded,
        metrics.query_attempts,
        metrics.nodes_queried,
        metrics.valid_responses,
        metrics.peers_discovered,
        metrics.nodes_discovered,
        metrics.malformed_responses,
        metrics.krpc_errors,
        metrics.uncorrelated_responses,
        metrics.timeouts,
        metrics.recovery_queries,
        metrics.recovery_peers,
        metrics.frontier_exhausted,
    );
    metrics
}

/// Iterative `get_peers` toward `infohash`, bounded by `budget`. Returns discovered peers.
pub async fn dht_get_peers(infohash: &[u8; 20], budget: Duration) -> Vec<SocketAddrV4> {
    dht_get_peers_with_target(infohash, budget, DHT_PEER_TARGET).await
}

pub async fn dht_get_peers_with_target(
    infohash: &[u8; 20],
    budget: Duration,
    peer_target: usize,
) -> Vec<SocketAddrV4> {
    dht_get_peers_incremental(infohash, budget, peer_target, |_| true).await
}

/// Publish each unique peer immediately after its source/transaction correlation succeeds.
/// Returning false from the observer cancels further harvesting. The complete set remains
/// available to aggregate callers; at most 1024 addresses are retained per walk.
pub async fn dht_get_peers_incremental(
    infohash: &[u8; 20],
    budget: Duration,
    peer_target: usize,
    mut on_peer: impl FnMut(SocketAddrV4) -> bool,
) -> Vec<SocketAddrV4> {
    let Ok(sock) = UdpSocket::bind("0.0.0.0:0").await else {
        return Vec::new();
    };
    let mut peers = BTreeSet::new();
    dht_walk(infohash, budget, &sock, |_src, response| {
        collect_response_peers(response, &mut peers, peer_target, &mut on_peer)
    })
    .await;
    peers.into_iter().collect()
}

fn collect_response_peers(
    response: &GetPeersResponse,
    peers: &mut BTreeSet<SocketAddrV4>,
    target: usize,
    on_peer: &mut impl FnMut(SocketAddrV4) -> bool,
) -> bool {
    for &peer in &response.peers {
        if peers.len() >= 1024 {
            return true;
        }
        if peers.insert(peer) && !on_peer(peer) {
            return true;
        }
    }
    peers.len() >= target.max(1)
}

#[cfg(test)]
async fn dht_get_peers_from_seeds(
    infohash: &[u8; 20],
    budget: Duration,
    target: usize,
    seeds: Vec<SocketAddrV4>,
) -> Vec<SocketAddrV4> {
    dht_peers_from_seeds_with_observer(infohash, budget, target, seeds, |_| true).await
}

#[cfg(test)]
async fn dht_peers_from_seeds_with_observer(
    infohash: &[u8; 20],
    budget: Duration,
    target: usize,
    seeds: Vec<SocketAddrV4>,
    mut observer: impl FnMut(SocketAddrV4) -> bool,
) -> Vec<SocketAddrV4> {
    let Ok(socket) = UdpSocket::bind("0.0.0.0:0").await else {
        return Vec::new();
    };
    let mut peers = BTreeSet::new();
    dht_walk_from_seeds(infohash, budget, &socket, seeds, |_, response| {
        collect_response_peers(response, &mut peers, target, &mut observer)
    })
    .await;
    peers.into_iter().collect()
}

#[cfg(test)]
async fn dht_stream_from_seeds(
    infohash: &[u8; 20],
    budget: Duration,
    target: usize,
    seeds: Vec<SocketAddrV4>,
    peers: tokio::sync::mpsc::Sender<SocketAddrV4>,
) {
    dht_peers_from_seeds_with_observer(infohash, budget, target, seeds, |peer| {
        peers.try_send(peer).is_ok()
    })
    .await;
}

/// The DHT half of self-announcement (BEP-5's `announce_peer`), never previously
/// implemented — `dht_get_peers` only ever read the swarm, never told it we're in it. Walks
/// toward `infohash` exactly like `dht_get_peers`, and for every node that hands us a
/// `get_peers` token (required — a node only accepts an announce echoing a token it itself
/// issued), sends it `announce_peer` for our `peer_port`. Makes outpace organically
/// discoverable via the DHT, not just tracker-discoverable (`announce_seeder`) — real
/// Acestream swarms are largely DHT-populated (see `README.md`).
///
/// Best-effort and fire-and-forget by nature (DHT is UDP): returns how many `announce_peer`
/// queries were sent, not a delivery/propagation guarantee.
pub async fn dht_announce_peer(infohash: &[u8; 20], peer_port: u16, budget: Duration) -> usize {
    let sock = match UdpSocket::bind("0.0.0.0:0").await {
        Ok(s) => s,
        Err(_) => return 0,
    };

    // Harvest (node, token) pairs during the walk; the actual announce send happens after,
    // back in async context (the walk's per-response callback is synchronous).
    let mut tokens: Vec<(SocketAddrV4, Vec<u8>)> = Vec::new();
    dht_walk(infohash, budget, &sock, |src, resp| {
        if let Some(token) = &resp.token {
            tokens.push((src, token.clone()));
        }
        tokens.len() >= 8
    })
    .await;

    let node_id: [u8; 20] = rand::random();
    let mut announced = 0usize;
    for (i, (addr, token)) in tokens.iter().enumerate() {
        let txid = (i as u16).to_be_bytes();
        let q = build_announce_peer(&node_id, infohash, peer_port, token, &txid);
        if sock.send_to(&q, SocketAddr::V4(*addr)).await.is_ok() {
            announced += 1;
        }
    }
    announced
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn bootstrap_resolution_control(slow: bool) {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::{Arc, Condvar, Mutex};
        let responder = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let seed = v4_addr(&responder);
        let peer: SocketAddrV4 = "127.0.0.1:43210".parse().unwrap();
        let gate = Arc::new((Mutex::new(false), Condvar::new()));
        let slow_started = Arc::new(AtomicUsize::new(0));
        let slow_finished = Arc::new(AtomicUsize::new(0));
        let resolver = Resolver::controlled({
            let gate = gate.clone();
            let started = slow_started.clone();
            let finished = slow_finished.clone();
            move |host| {
                if host.starts_with("slow.") {
                    started.fetch_add(1, Ordering::SeqCst);
                    let mut released = gate.0.lock().unwrap();
                    while !*released {
                        released = gate.1.wait(released).unwrap();
                    }
                    finished.fetch_add(1, Ordering::SeqCst);
                }
                Ok(vec![SocketAddr::V4(seed)])
            }
        });
        let server = tokio::spawn(async move {
            let mut bytes = [0; 2048];
            let (n, caller) = responder.recv_from(&mut bytes).await.unwrap();
            let reply = response_with(&buf_txid(&bytes[..n]), &[peer], &[]);
            responder.send_to(&reply, caller).await.unwrap();
        });
        let socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let (publication, mut received) = tokio::sync::mpsc::channel(1);
        let terminal_resolver = resolver.clone();
        let run = tokio::spawn(async move {
            let hosts: &[&str] = if slow {
                &["healthy.invalid:1", "slow.invalid:1"]
            } else {
                &["healthy.invalid:1"]
            };
            dht_walk_with_bootstraps(
                &[0; 20],
                Duration::from_millis(500),
                &socket,
                hosts,
                resolver,
                None,
                |_, response| {
                    if response.peers.contains(&peer) {
                        let _ = publication.try_send(peer);
                        return true;
                    }
                    false
                },
            )
            .await
        });
        let prompt = tokio::time::timeout(Duration::from_millis(200), received.recv()).await;
        let slow_pending = !slow
            || (slow_started.load(Ordering::SeqCst) == 1
                && slow_finished.load(Ordering::SeqCst) == 0);
        let terminal = tokio::time::timeout(Duration::from_millis(800), run)
            .await
            .unwrap()
            .unwrap();
        *gate.0.lock().unwrap() = true;
        gate.1.notify_all();
        tokio::time::timeout(Duration::from_secs(2), async {
            while slow_finished.load(Ordering::SeqCst) != slow_started.load(Ordering::SeqCst) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        terminal_resolver.finish_controlled().await;
        server.abort();
        let _ = server.await;
        assert!(
            slow_pending,
            "slow native lookup was not outstanding at the measured publication boundary"
        );
        assert!(matches!(prompt, Ok(Some(found)) if found == peer), "healthy resolved bootstrap received no prompt correlated query/publication while another lookup remained pending; queried={}", terminal.nodes_queried);
    }
    #[tokio::test]
    async fn healthy_bootstrap_publishes_before_slow_resolution_finishes() {
        bootstrap_resolution_control(true).await;
    }
    #[tokio::test]
    async fn healthy_bootstrap_resolution_positive_control() {
        bootstrap_resolution_control(false).await;
    }

    #[tokio::test]
    async fn late_bootstrap_joins_the_same_walk_after_first_seed_is_unproductive() {
        use std::sync::{Arc, Condvar, Mutex};
        let silent = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let healthy = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let silent_addr = v4_addr(&silent);
        let healthy_addr = v4_addr(&healthy);
        let peer: SocketAddrV4 = "127.0.0.1:43211".parse().unwrap();
        let gate = Arc::new((Mutex::new(false), Condvar::new()));
        let resolver = Resolver::controlled({
            let gate = gate.clone();
            move |host| {
                if host.starts_with("late.") {
                    let mut released = gate.0.lock().unwrap();
                    while !*released {
                        released = gate.1.wait(released).unwrap();
                    }
                    Ok(vec![SocketAddr::V4(healthy_addr)])
                } else {
                    Ok(vec![SocketAddr::V4(silent_addr)])
                }
            }
        });
        let release_gate = gate.clone();
        let servers = tokio::spawn(async move {
            let mut bytes = [0; 2048];
            // Resolve the late useful seed only once the first real UDP query was sent.
            silent.recv_from(&mut bytes).await.unwrap();
            *gate.0.lock().unwrap() = true;
            gate.1.notify_all();
            let (n, caller) = healthy.recv_from(&mut bytes).await.unwrap();
            healthy
                .send_to(&response_with(&buf_txid(&bytes[..n]), &[peer], &[]), caller)
                .await
                .unwrap();
        });
        let socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let started = Instant::now();
        let mut published = false;
        let metrics = dht_walk_with_bootstraps(
            &[0; 20],
            Duration::from_millis(500),
            &socket,
            &["first.invalid:1", "late.invalid:1"],
            resolver.clone(),
            None,
            |_, response| {
                published |= response.peers.contains(&peer);
                published
            },
        )
        .await;
        let elapsed = started.elapsed();
        *release_gate.0.lock().unwrap() = true;
        release_gate.1.notify_all();
        servers.abort();
        let _ = servers.await;
        resolver.finish_controlled().await;
        assert!(
            published,
            "later bootstrap was lost after first unproductive seed"
        );
        assert_eq!(metrics.bootstrap_seeded, 2);
        assert_eq!(metrics.nodes_queried, 2);
        assert_eq!(metrics.valid_responses, 1);
        assert!(
            elapsed < Duration::from_millis(300),
            "late seed waited for a renewed/serial response budget"
        );
    }

    #[test]
    fn get_peers_query_roundtrips() {
        let q = build_get_peers(&[1u8; 20], &[2u8; 20], b"aa");
        let d = Bencode::parse(&q).unwrap();
        assert_eq!(
            d.get(b"q").unwrap().as_bytes(),
            Some(b"get_peers".as_slice())
        );
        assert_eq!(d.get(b"y").unwrap().as_bytes(), Some(b"q".as_slice()));
        let a = d.get(b"a").unwrap();
        assert_eq!(
            a.get(b"info_hash").unwrap().as_bytes(),
            Some([2u8; 20].as_slice())
        );
    }

    #[test]
    fn announce_peer_query_roundtrips() {
        let q = build_announce_peer(&[1u8; 20], &[2u8; 20], 8621, b"tok123", b"aa");
        let d = Bencode::parse(&q).unwrap();
        assert_eq!(
            d.get(b"q").unwrap().as_bytes(),
            Some(b"announce_peer".as_slice())
        );
        assert_eq!(d.get(b"y").unwrap().as_bytes(), Some(b"q".as_slice()));
        let a = d.get(b"a").unwrap();
        assert_eq!(
            a.get(b"info_hash").unwrap().as_bytes(),
            Some([2u8; 20].as_slice())
        );
        assert_eq!(a.get(b"id").unwrap().as_bytes(), Some([1u8; 20].as_slice()));
        assert_eq!(a.get(b"port").unwrap().as_int(), Some(8621));
        assert_eq!(
            a.get(b"token").unwrap().as_bytes(),
            Some(b"tok123".as_slice())
        );
        assert_eq!(a.get(b"implied_port").unwrap().as_int(), Some(0));
    }

    #[test]
    fn parses_values_peers() {
        // r = { id:20, token:.., values: [ "<4 ip><2 port>" ] }
        let mut r: BTreeMap<Vec<u8>, Bencode> = BTreeMap::new();
        r.insert(b"id".to_vec(), Bencode::Bytes(vec![0; 20]));
        let peer = vec![87u8, 221, 96, 148, 0x21, 0xAD]; // 87.221.96.148:8621
        r.insert(
            b"values".to_vec(),
            Bencode::List(vec![Bencode::Bytes(peer)]),
        );
        let mut top: BTreeMap<Vec<u8>, Bencode> = BTreeMap::new();
        top.insert(b"r".to_vec(), Bencode::Dict(r));
        top.insert(b"y".to_vec(), Bencode::Bytes(b"r".to_vec()));
        let resp = parse_response(&Bencode::Dict(top).encode()).unwrap();
        assert_eq!(resp.peers, vec!["87.221.96.148:8621".parse().unwrap()]);
    }

    #[test]
    fn parses_token_needed_to_announce_back() {
        let mut r: BTreeMap<Vec<u8>, Bencode> = BTreeMap::new();
        r.insert(b"id".to_vec(), Bencode::Bytes(vec![0; 20]));
        r.insert(b"token".to_vec(), Bencode::Bytes(b"opaque-tok".to_vec()));
        let mut top: BTreeMap<Vec<u8>, Bencode> = BTreeMap::new();
        top.insert(b"r".to_vec(), Bencode::Dict(r));
        top.insert(b"y".to_vec(), Bencode::Bytes(b"r".to_vec()));
        let resp = parse_response(&Bencode::Dict(top).encode()).unwrap();
        assert_eq!(resp.token, Some(b"opaque-tok".to_vec()));
    }

    #[test]
    fn no_token_parses_as_none() {
        let mut r: BTreeMap<Vec<u8>, Bencode> = BTreeMap::new();
        r.insert(b"id".to_vec(), Bencode::Bytes(vec![0; 20]));
        let mut top: BTreeMap<Vec<u8>, Bencode> = BTreeMap::new();
        top.insert(b"r".to_vec(), Bencode::Dict(r));
        top.insert(b"y".to_vec(), Bencode::Bytes(b"r".to_vec()));
        let resp = parse_response(&Bencode::Dict(top).encode()).unwrap();
        assert_eq!(resp.token, None);
    }

    #[test]
    fn parses_compact_nodes() {
        let mut node = vec![9u8; 20]; // node id
        node.extend_from_slice(&[1, 2, 3, 4, 0x1A, 0xE1]); // 1.2.3.4:6881
        let mut r: BTreeMap<Vec<u8>, Bencode> = BTreeMap::new();
        r.insert(b"nodes".to_vec(), Bencode::Bytes(node));
        let mut top: BTreeMap<Vec<u8>, Bencode> = BTreeMap::new();
        top.insert(b"r".to_vec(), Bencode::Dict(r));
        top.insert(b"y".to_vec(), Bencode::Bytes(b"r".to_vec()));
        let resp = parse_response(&Bencode::Dict(top).encode()).unwrap();
        assert_eq!(resp.nodes.len(), 1);
        assert_eq!(resp.nodes[0].1, "1.2.3.4:6881".parse().unwrap());
    }

    // --- Malformed / hostile KRPC response hardening (#41) ---
    //
    // Public DHT UDP is untrusted, so `parse_response` must ignore anything that isn't a
    // well-formed `get_peers` *response*, rather than mining peers/nodes/tokens out of it.

    /// Build a raw top-level KRPC message: an optional `y` (message type), a `t`
    /// transaction id, and the given `r` value — which need not be a dict, so malformed
    /// shapes can be exercised.
    fn krpc_message(y: Option<&[u8]>, r: Bencode) -> Vec<u8> {
        let mut top: BTreeMap<Vec<u8>, Bencode> = BTreeMap::new();
        top.insert(b"t".to_vec(), Bencode::Bytes(b"aa".to_vec()));
        if let Some(y) = y {
            top.insert(b"y".to_vec(), Bencode::Bytes(y.to_vec()));
        }
        top.insert(b"r".to_vec(), r);
        Bencode::Dict(top).encode()
    }

    /// Build an `r` response dict from optional `values`/`nodes`/`token` parts.
    fn r_dict(values: Option<Bencode>, nodes: Option<Bencode>, token: Option<Bencode>) -> Bencode {
        let mut r: BTreeMap<Vec<u8>, Bencode> = BTreeMap::new();
        r.insert(b"id".to_vec(), Bencode::Bytes(vec![7; 20]));
        if let Some(v) = values {
            r.insert(b"values".to_vec(), v);
        }
        if let Some(n) = nodes {
            r.insert(b"nodes".to_vec(), n);
        }
        if let Some(t) = token {
            r.insert(b"token".to_vec(), t);
        }
        Bencode::Dict(r)
    }

    /// A `values` list carrying exactly one valid compact peer (10.0.0.1:1111).
    fn one_valid_peer_values() -> Bencode {
        Bencode::List(vec![Bencode::Bytes(vec![10, 0, 0, 1, 0x04, 0x57])])
    }

    #[test]
    fn ignores_message_whose_type_is_not_a_response() {
        // y = "q" (a query), not "r". A hostile packet can still smuggle an `r` dict with
        // peers and a token; gating on the message type keeps those from being trusted.
        let r = r_dict(
            Some(one_valid_peer_values()),
            None,
            Some(Bencode::Bytes(b"tok".to_vec())),
        );
        assert!(parse_response(&krpc_message(Some(b"q"), r)).is_none());
    }

    #[test]
    fn ignores_response_with_missing_message_type() {
        // BEP-5 requires `y`; a message that omits it is malformed and must be ignored.
        let r = r_dict(Some(one_valid_peer_values()), None, None);
        assert!(parse_response(&krpc_message(None, r)).is_none());
    }

    #[test]
    fn ignores_error_message() {
        // KRPC error: y="e", e=[code, msg], no `r`.
        let mut top: BTreeMap<Vec<u8>, Bencode> = BTreeMap::new();
        top.insert(b"t".to_vec(), Bencode::Bytes(b"aa".to_vec()));
        top.insert(b"y".to_vec(), Bencode::Bytes(b"e".to_vec()));
        top.insert(
            b"e".to_vec(),
            Bencode::List(vec![Bencode::Int(201), Bencode::Bytes(b"err".to_vec())]),
        );
        assert!(parse_response(&Bencode::Dict(top).encode()).is_none());
    }

    #[test]
    fn ignores_response_whose_r_is_not_a_dict() {
        // y="r" but `r` is a byte string, not the expected dict — reject deterministically
        // instead of silently treating it as an empty response.
        let raw = krpc_message(Some(b"r"), Bencode::Bytes(b"not-a-dict".to_vec()));
        assert!(parse_response(&raw).is_none());
    }

    #[test]
    fn ignores_partial_trailing_compact_node_record() {
        // One full 26-byte node followed by a truncated fragment: the fragment is dropped,
        // the valid node survives.
        let mut nodes = vec![9u8; 20];
        nodes.extend_from_slice(&[1, 2, 3, 4, 0x1A, 0xE1]); // 1.2.3.4:6881
        nodes.extend_from_slice(&[0xAB; 10]); // partial record
        let r = r_dict(None, Some(Bencode::Bytes(nodes)), None);
        let resp = parse_response(&krpc_message(Some(b"r"), r)).unwrap();
        assert_eq!(resp.nodes.len(), 1);
        assert_eq!(resp.nodes[0].1, "1.2.3.4:6881".parse().unwrap());
    }

    #[test]
    fn skips_invalid_compact_peer_values() {
        // Wrong-length entries, a port-0 entry, and a non-bytes entry are all skipped;
        // only the one valid compact peer survives.
        let vals = Bencode::List(vec![
            Bencode::Bytes(vec![10, 0, 0, 5]),       // too short (4 bytes)
            Bencode::Bytes(vec![10, 0, 0, 6, 0, 0]), // port 0
            Bencode::Bytes(vec![10, 0, 0, 7, 0x04, 0x57, 0xFF]), // too long (7 bytes)
            Bencode::Int(42),                        // not bytes at all
            Bencode::Bytes(vec![10, 0, 0, 1, 0x04, 0x57]), // valid -> 10.0.0.1:1111
        ]);
        let r = r_dict(Some(vals), None, None);
        let resp = parse_response(&krpc_message(Some(b"r"), r)).unwrap();
        assert_eq!(resp.peers, vec!["10.0.0.1:1111".parse().unwrap()]);
    }

    #[test]
    fn ignores_values_that_are_not_a_list() {
        // `values` as a byte string (not a list) yields no peers.
        let r = r_dict(
            Some(Bencode::Bytes(vec![10, 0, 0, 1, 0x04, 0x57])),
            None,
            None,
        );
        let resp = parse_response(&krpc_message(Some(b"r"), r)).unwrap();
        assert!(resp.peers.is_empty());
    }

    #[test]
    fn ignores_nodes_that_are_not_bytes() {
        // `nodes` as a list (not the compact byte string) yields no nodes.
        let r = r_dict(None, Some(Bencode::List(vec![Bencode::Int(1)])), None);
        let resp = parse_response(&krpc_message(Some(b"r"), r)).unwrap();
        assert!(resp.nodes.is_empty());
    }

    #[test]
    fn ignores_token_that_is_not_bytes() {
        // `token` as an int (not bytes) is treated as absent, not coerced.
        let r = r_dict(None, None, Some(Bencode::Int(42)));
        let resp = parse_response(&krpc_message(Some(b"r"), r)).unwrap();
        assert_eq!(resp.token, None);
    }

    #[test]
    fn drops_payload_truncated_at_the_receive_buffer_boundary() {
        // A datagram larger than the 2048-byte receive buffer arrives truncated; the
        // remaining bencode is incomplete and must be rejected, not partially trusted.
        let big_token = vec![0x5A; 4096];
        let r = r_dict(
            Some(one_valid_peer_values()),
            None,
            Some(Bencode::Bytes(big_token)),
        );
        let raw = krpc_message(Some(b"r"), r);
        assert!(raw.len() > 2048, "fixture must exceed the receive buffer");
        assert!(parse_response(&raw[..2048]).is_none());
    }

    #[test]
    fn distance_is_zero_to_self_and_orders() {
        assert_eq!(distance(&[5; 20], &[5; 20]), [0; 20]);
        assert!(distance(&[0; 20], &[1; 20]) < distance(&[0xff; 20], &[0; 20]));
    }

    /// Extract the KRPC transaction id (`t`) from a raw query/response buffer.
    pub(super) fn buf_txid(buf: &[u8]) -> Vec<u8> {
        Bencode::parse(buf)
            .unwrap()
            .get(b"t")
            .and_then(Bencode::as_bytes)
            .expect("query must carry a transaction id")
            .to_vec()
    }

    /// Build a `get_peers` `values` response echoing `txid` and carrying `peers`.
    fn values_response(txid: &[u8], peers: &[SocketAddrV4]) -> Vec<u8> {
        let vals: Vec<Bencode> = peers
            .iter()
            .map(|p| {
                let mut b = Vec::new();
                b.extend_from_slice(&p.ip().octets());
                b.extend_from_slice(&p.port().to_be_bytes());
                Bencode::Bytes(b)
            })
            .collect();
        let mut r: BTreeMap<Vec<u8>, Bencode> = BTreeMap::new();
        r.insert(b"id".to_vec(), Bencode::Bytes(vec![7; 20]));
        r.insert(b"values".to_vec(), Bencode::List(vals));
        let mut top: BTreeMap<Vec<u8>, Bencode> = BTreeMap::new();
        top.insert(b"t".to_vec(), Bencode::Bytes(txid.to_vec()));
        top.insert(b"y".to_vec(), Bencode::Bytes(b"r".to_vec()));
        top.insert(b"r".to_vec(), Bencode::Dict(r));
        Bencode::Dict(top).encode()
    }

    #[test]
    fn parse_response_extracts_transaction_id() {
        // The transaction id lives at the top level (`t`), not inside `r`; the walk needs it
        // to correlate a reply against the query it answers.
        let resp = values_response(b"Zx", &["10.0.0.9:9000".parse().unwrap()]);
        let got = parse_response(&resp).unwrap();
        assert_eq!(got.txid, b"Zx".to_vec());
    }

    #[tokio::test]
    async fn dht_stream_publishes_first_correlated_reply_before_later_reply() {
        let mut servers = tokio::task::JoinSet::new();
        let mut seeds = Vec::new();
        for (i, delay) in [Duration::ZERO, Duration::from_millis(600)]
            .into_iter()
            .enumerate()
        {
            let socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();
            let SocketAddr::V4(seed) = socket.local_addr().unwrap() else {
                unreachable!()
            };
            seeds.push(seed);
            servers.spawn(async move {
                let mut buf = [0; 2048];
                let (n, client) = socket.recv_from(&mut buf).await.unwrap();
                let txid = buf_txid(&buf[..n]);
                tokio::time::sleep(delay).await;
                let peer = SocketAddrV4::new(Ipv4Addr::LOCALHOST, 1000 + i as u16);
                socket
                    .send_to(&values_response(&txid, &[peer]), client)
                    .await
                    .unwrap();
            });
        }
        let (tx, mut rx) = tokio::sync::mpsc::channel(8);
        let walk = tokio::spawn(async move {
            dht_stream_from_seeds(&[9; 20], Duration::from_secs(2), 2, seeds, tx).await;
        });
        let first = tokio::time::timeout(Duration::from_millis(250), rx.recv()).await;
        if first.is_err() {
            walk.abort();
            servers.abort_all();
        }
        if let Ok(first) = &first {
            let second = tokio::time::timeout(Duration::from_secs(1), rx.recv())
                .await
                .unwrap()
                .unwrap();
            assert_ne!(first.as_ref().unwrap(), &second);
        }
        let _ = walk.await;
        while servers.join_next().await.is_some() {}
        assert!(
            first.is_ok(),
            "first correlated peer was hidden until the walk completed"
        );
    }

    #[tokio::test]
    async fn incremental_observer_rejects_wrong_txid_source_replay_and_duplicate_values() {
        let socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let spoof = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let SocketAddr::V4(seed) = socket.local_addr().unwrap() else {
            unreachable!()
        };
        let expected = SocketAddrV4::new(Ipv4Addr::LOCALHOST, 1234);
        let server = tokio::spawn(async move {
            let mut bytes = [0; 2048];
            let (n, client) = socket.recv_from(&mut bytes).await.unwrap();
            let txid = buf_txid(&bytes[..n]);
            let mut wrong = txid.clone();
            wrong.push(0xff);
            let bad = SocketAddrV4::new(Ipv4Addr::LOCALHOST, 4321);
            socket
                .send_to(&values_response(&wrong, &[bad]), client)
                .await
                .unwrap();
            spoof
                .send_to(&values_response(&txid, &[bad]), client)
                .await
                .unwrap();
            socket
                .send_to(&values_response(&txid, &[expected, expected]), client)
                .await
                .unwrap();
            socket
                .send_to(&values_response(&txid, &[bad]), client)
                .await
                .unwrap();
        });
        let (sender, mut receiver) = tokio::sync::mpsc::channel(8);
        dht_stream_from_seeds(&[0; 20], Duration::from_millis(150), 8, vec![seed], sender).await;
        server.await.unwrap();
        assert_eq!(receiver.recv().await, Some(expected));
        assert!(
            receiver.recv().await.is_none(),
            "uncorrelated/replayed/duplicate peer reached production observer"
        );
    }

    #[tokio::test]
    async fn dht_response_window_respects_remaining_walk_budget() {
        let server = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let SocketAddr::V4(seed) = server.local_addr().unwrap() else {
            unreachable!()
        };
        let start = Instant::now();
        let peers =
            dht_get_peers_from_seeds(&[9; 20], Duration::from_millis(40), 8, vec![seed]).await;
        let elapsed = start.elapsed();
        assert!(peers.is_empty());
        let mut query = [0; 2048];
        assert!(
            server.try_recv_from(&mut query).is_ok(),
            "actual UDP query required"
        );
        assert!(
            elapsed < Duration::from_millis(250),
            "40ms walk consumed {elapsed:?}"
        );
    }

    #[tokio::test]
    async fn dht_lookup_stops_once_peer_target_is_met() {
        let server = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let seed = match server.local_addr().unwrap() {
            SocketAddr::V4(v4) => v4,
            _ => panic!("test server must be IPv4"),
        };
        let handle = tokio::spawn(async move {
            let mut buf = [0u8; 2048];
            let (n, peer) = server.recv_from(&mut buf).await.unwrap();
            // A real node echoes the query's transaction id; the client now requires it.
            let txid = buf_txid(&buf[..n]);
            let resp = values_response(
                &txid,
                &[
                    "10.0.0.1:1111".parse().unwrap(),
                    "10.0.0.2:2222".parse().unwrap(),
                ],
            );
            server.send_to(&resp, peer).await.unwrap();
        });

        let start = Instant::now();
        let peers =
            dht_get_peers_from_seeds(&[9u8; 20], Duration::from_secs(5), 2, vec![seed]).await;
        assert_eq!(
            peers,
            vec![
                "10.0.0.1:1111".parse().unwrap(),
                "10.0.0.2:2222".parse().unwrap()
            ]
        );
        assert!(
            start.elapsed() < Duration::from_secs(2),
            "lookup should return as soon as enough peers are available"
        );
        handle.await.unwrap();
    }

    #[tokio::test]
    async fn dht_ignores_response_with_wrong_transaction_id() {
        // A valid-looking response from the queried node but echoing a transaction id we never
        // sent must not contribute peers — it is unrelated (or forged) traffic.
        let server = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let seed = match server.local_addr().unwrap() {
            SocketAddr::V4(v4) => v4,
            _ => panic!("test server must be IPv4"),
        };
        let handle = tokio::spawn(async move {
            let mut buf = [0u8; 2048];
            let (n, peer) = server.recv_from(&mut buf).await.unwrap();
            let real = buf_txid(&buf[..n]);
            // Deliberately corrupt the echoed txid so it cannot match any inflight query.
            let mut wrong = real.clone();
            wrong.push(0xff);
            let resp = values_response(&wrong, &["10.0.0.1:1111".parse().unwrap()]);
            server.send_to(&resp, peer).await.unwrap();
        });

        let peers =
            dht_get_peers_from_seeds(&[9u8; 20], Duration::from_millis(1200), 2, vec![seed]).await;
        assert!(
            peers.is_empty(),
            "response with a mismatched transaction id must be ignored, got {peers:?}"
        );
        handle.await.unwrap();
    }

    #[tokio::test]
    async fn dht_ignores_response_from_wrong_source() {
        // A response echoing the correct transaction id but arriving from an address we never
        // queried must be ignored: an off-path attacker who guesses/observes the txid still
        // cannot inject peers unless the source also matches the inflight query.
        let server = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let spoofer = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let seed = match server.local_addr().unwrap() {
            SocketAddr::V4(v4) => v4,
            _ => panic!("test server must be IPv4"),
        };
        let handle = tokio::spawn(async move {
            let mut buf = [0u8; 2048];
            // The queried node stays silent; the spoofer (different source) replies with the
            // correct txid it observed from the query.
            let (n, peer) = server.recv_from(&mut buf).await.unwrap();
            let txid = buf_txid(&buf[..n]);
            let resp = values_response(&txid, &["10.0.0.1:1111".parse().unwrap()]);
            spoofer.send_to(&resp, peer).await.unwrap();
        });

        let peers =
            dht_get_peers_from_seeds(&[9u8; 20], Duration::from_millis(1200), 2, vec![seed]).await;
        assert!(
            peers.is_empty(),
            "response from an unqueried source must be ignored, got {peers:?}"
        );
        handle.await.unwrap();
    }

    // --- Walk diagnostics / metrics (#43) ---
    //
    // A raw peer count hides useful failure modes (silence, malformed traffic, KRPC errors,
    // uncorrelated packets, frontier exhaustion). `dht_walk_from_seeds` returns a
    // `DhtWalkMetrics` so those modes are observable and deterministically testable offline.

    /// Build a `get_peers` response echoing `txid` with the given compact peers and nodes.
    pub(super) fn response_with(
        txid: &[u8],
        peers: &[SocketAddrV4],
        nodes: &[([u8; 20], SocketAddrV4)],
    ) -> Vec<u8> {
        let vals: Vec<Bencode> = peers
            .iter()
            .map(|p| {
                let mut b = Vec::new();
                b.extend_from_slice(&p.ip().octets());
                b.extend_from_slice(&p.port().to_be_bytes());
                Bencode::Bytes(b)
            })
            .collect();
        let mut node_bytes = Vec::new();
        for (id, addr) in nodes {
            node_bytes.extend_from_slice(id);
            node_bytes.extend_from_slice(&addr.ip().octets());
            node_bytes.extend_from_slice(&addr.port().to_be_bytes());
        }
        let mut r: BTreeMap<Vec<u8>, Bencode> = BTreeMap::new();
        r.insert(b"id".to_vec(), Bencode::Bytes(vec![7; 20]));
        if !peers.is_empty() {
            r.insert(b"values".to_vec(), Bencode::List(vals));
        }
        if !nodes.is_empty() {
            r.insert(b"nodes".to_vec(), Bencode::Bytes(node_bytes));
        }
        let mut top: BTreeMap<Vec<u8>, Bencode> = BTreeMap::new();
        top.insert(b"t".to_vec(), Bencode::Bytes(txid.to_vec()));
        top.insert(b"y".to_vec(), Bencode::Bytes(b"r".to_vec()));
        top.insert(b"r".to_vec(), Bencode::Dict(r));
        Bencode::Dict(top).encode()
    }

    /// Build a KRPC error message (`y == "e"`) echoing `txid`.
    pub(super) fn error_response(txid: &[u8]) -> Vec<u8> {
        let mut top: BTreeMap<Vec<u8>, Bencode> = BTreeMap::new();
        top.insert(b"t".to_vec(), Bencode::Bytes(txid.to_vec()));
        top.insert(b"y".to_vec(), Bencode::Bytes(b"e".to_vec()));
        top.insert(
            b"e".to_vec(),
            Bencode::List(vec![Bencode::Int(201), Bencode::Bytes(b"err".to_vec())]),
        );
        Bencode::Dict(top).encode()
    }

    pub(super) fn v4_addr(sock: &UdpSocket) -> SocketAddrV4 {
        match sock.local_addr().unwrap() {
            SocketAddr::V4(v4) => v4,
            _ => panic!("test socket must be IPv4"),
        }
    }

    #[tokio::test]
    async fn walk_metrics_count_a_successful_response_path() {
        let server = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let seed = v4_addr(&server);
        let handle = tokio::spawn(async move {
            let mut buf = [0u8; 2048];
            let (n, peer) = server.recv_from(&mut buf).await.unwrap();
            let txid = buf_txid(&buf[..n]);
            let resp = response_with(
                &txid,
                &[
                    "10.0.0.1:1111".parse().unwrap(),
                    "10.0.0.2:2222".parse().unwrap(),
                ],
                &[([9u8; 20], "1.2.3.4:6881".parse().unwrap())],
            );
            server.send_to(&resp, peer).await.unwrap();
        });

        let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let mut seen: BTreeSet<SocketAddrV4> = BTreeSet::new();
        let metrics = dht_walk_from_seeds(
            &[9u8; 20],
            Duration::from_secs(5),
            &client,
            vec![seed],
            |_src, resp| {
                seen.extend(resp.peers.iter().copied());
                seen.len() >= 2
            },
        )
        .await;

        assert_eq!(metrics.bootstrap_seeded, 1);
        assert_eq!(metrics.nodes_queried, 1);
        assert_eq!(metrics.valid_responses, 1);
        assert_eq!(metrics.peers_discovered, 2);
        assert_eq!(metrics.nodes_discovered, 1);
        assert_eq!(metrics.malformed_responses, 0);
        assert_eq!(metrics.krpc_errors, 0);
        assert_eq!(metrics.uncorrelated_responses, 0);
        assert_eq!(metrics.timeouts, 0);
        handle.await.unwrap();
    }

    #[tokio::test]
    async fn walk_metrics_count_ignored_malformed_and_error_traffic() {
        // One seed replies with a well-formed KRPC error, another with non-bencode garbage;
        // neither yields peers, and each is bucketed by its own failure mode.
        let err_server = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let garbage_server = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let err_seed = v4_addr(&err_server);
        let garbage_seed = v4_addr(&garbage_server);

        let h1 = tokio::spawn(async move {
            let mut buf = [0u8; 2048];
            let (n, peer) = err_server.recv_from(&mut buf).await.unwrap();
            let txid = buf_txid(&buf[..n]);
            err_server
                .send_to(&error_response(&txid), peer)
                .await
                .unwrap();
        });
        let h2 = tokio::spawn(async move {
            let mut buf = [0u8; 2048];
            let (_n, peer) = garbage_server.recv_from(&mut buf).await.unwrap();
            garbage_server
                .send_to(b"not-a-bencode-datagram", peer)
                .await
                .unwrap();
        });

        let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let metrics = dht_walk_from_seeds(
            &[9u8; 20],
            Duration::from_secs(5),
            &client,
            vec![err_seed, garbage_seed],
            |_src, _resp| false,
        )
        .await;

        assert_eq!(metrics.valid_responses, 0);
        assert_eq!(metrics.peers_discovered, 0);
        assert_eq!(metrics.krpc_errors, 1);
        assert_eq!(metrics.malformed_responses, 1);
        assert!(metrics.frontier_exhausted);
        h1.await.unwrap();
        h2.await.unwrap();
    }

    #[tokio::test]
    async fn walk_metrics_count_a_silent_query_as_a_timeout() {
        // A silent seed releases its slot at the soft timeout, and the walk exhausts
        // its frontier without waiting for the full response-correlation TTL.
        let server = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let seed = v4_addr(&server);
        // Keep `server` bound (so the port stays reachable, no ICMP unreachable) but silent.

        let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let metrics = dht_walk_from_seeds(
            &[9u8; 20],
            Duration::from_secs(5),
            &client,
            vec![seed],
            |_src, _resp| false,
        )
        .await;

        assert_eq!(metrics.valid_responses, 0);
        assert_eq!(metrics.timeouts, 1);
        assert!(metrics.frontier_exhausted);
        drop(server);
    }

    #[tokio::test]
    async fn walk_metrics_count_an_uncorrelated_response() {
        // Correct source, wrong transaction id: the datagram arrives (so it is not a timeout)
        // but fails correlation, so it contributes no peers and is bucketed as uncorrelated.
        let server = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let seed = v4_addr(&server);
        let handle = tokio::spawn(async move {
            let mut buf = [0u8; 2048];
            let (n, peer) = server.recv_from(&mut buf).await.unwrap();
            let mut wrong = buf_txid(&buf[..n]);
            wrong.push(0xff);
            let resp = response_with(&wrong, &["10.0.0.1:1111".parse().unwrap()], &[]);
            server.send_to(&resp, peer).await.unwrap();
        });

        let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let metrics = dht_walk_from_seeds(
            &[9u8; 20],
            Duration::from_secs(5),
            &client,
            vec![seed],
            |_src, _resp| false,
        )
        .await;

        assert_eq!(metrics.valid_responses, 0);
        assert_eq!(metrics.peers_discovered, 0);
        assert_eq!(metrics.uncorrelated_responses, 1);
        assert_eq!(metrics.timeouts, 0);
        handle.await.unwrap();
    }

    // Live DHT lookup against a real infohash:
    //   ACE_INFOHASH=50e935...2d6e47 cargo test -p ace-swarm dht_live -- --ignored --nocapture
    #[tokio::test]
    #[ignore]
    async fn dht_live_finds_peers() {
        let hex = std::env::var("ACE_INFOHASH").expect("set ACE_INFOHASH=40hex");
        let mut ih = [0u8; 20];
        for i in 0..20 {
            ih[i] = u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16).unwrap();
        }
        let peers = dht_get_peers(&ih, std::time::Duration::from_secs(20)).await;
        println!("DHT found {} peer(s):", peers.len());
        for p in &peers {
            println!("  {p}");
        }
        assert!(!peers.is_empty(), "DHT returned no peers");
    }

    // Live DHT self-announce against a real infohash — confirms we can get tokens from real
    // nodes and send announce_peer without erroring. Doesn't (can't, from one host) prove a
    // third party subsequently finds us; that's what Task 7's reverse-direction proof needs.
    //   ACE_INFOHASH=50e935...2d6e47 cargo test -p ace-swarm dht_live_announce -- --ignored --nocapture
    #[tokio::test]
    #[ignore]
    async fn dht_live_announce_sends_without_erroring() {
        let hex = std::env::var("ACE_INFOHASH").expect("set ACE_INFOHASH=40hex");
        let mut ih = [0u8; 20];
        for i in 0..20 {
            ih[i] = u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16).unwrap();
        }
        let announced = dht_announce_peer(&ih, 8621, std::time::Duration::from_secs(20)).await;
        println!("DHT announce_peer sent to {announced} node(s)");
        assert!(
            announced > 0,
            "no node handed us a token to announce back with"
        );
    }
}

#[cfg(test)]
mod recovery_tests;
