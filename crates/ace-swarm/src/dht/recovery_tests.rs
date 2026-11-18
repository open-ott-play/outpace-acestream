//! Local UDP regressions for response pacing and one bounded recovery pass.
use super::tests::{buf_txid, error_response, response_with, v4_addr};
use super::*;

type AskedLog = std::sync::Arc<std::sync::Mutex<Vec<Vec<u8>>>>;

fn answer_once(
    server: UdpSocket,
    reply: impl FnOnce(&[u8]) -> Vec<u8> + Send + 'static,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut buf = [0u8; 2048];
        let (n, caller) = server.recv_from(&mut buf).await.unwrap();
        let resp = reply(&buf_txid(&buf[..n]));
        server.send_to(&resp, caller).await.unwrap();
        // Keep the port bound until the walk is done with it.
        std::future::pending::<()>().await
    })
}

async fn bind_loopback(n: usize) -> Vec<UdpSocket> {
    let mut out = Vec::with_capacity(n);
    for _ in 0..n {
        out.push(UdpSocket::bind("127.0.0.1:0").await.unwrap());
    }
    out
}

fn error_with_payload(txid: &[u8], e: Option<Bencode>) -> Vec<u8> {
    let mut top: BTreeMap<Vec<u8>, Bencode> = BTreeMap::new();
    top.insert(b"t".to_vec(), Bencode::Bytes(txid.to_vec()));
    top.insert(b"y".to_vec(), Bencode::Bytes(b"e".to_vec()));
    if let Some(e) = e {
        top.insert(b"e".to_vec(), e);
    }
    Bencode::Dict(top).encode()
}

fn query_method(buf: &[u8]) -> Vec<u8> {
    Bencode::parse(buf)
        .ok()
        .and_then(|v| v.get(b"q").and_then(Bencode::as_bytes).map(<[u8]>::to_vec))
        .unwrap_or_default()
}

fn scripted_router(
    server: UdpSocket,
    reply: impl Fn(usize, &[u8], &[u8]) -> Option<Vec<u8>> + Send + 'static,
) -> (tokio::task::JoinHandle<()>, AskedLog) {
    let asked = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let log = asked.clone();
    let handle = tokio::spawn(async move {
        let mut buf = [0u8; 2048];
        for n in 0.. {
            let Ok((len, caller)) = server.recv_from(&mut buf).await else {
                return;
            };
            let method = query_method(&buf[..len]);
            log.lock().unwrap().push(method.clone());
            if let Some(resp) = reply(n, &method, &buf_txid(&buf[..len])) {
                let _ = server.send_to(&resp, caller).await;
            }
        }
    });
    (handle, asked)
}

fn addr(last: u8) -> SocketAddrV4 {
    SocketAddrV4::new(Ipv4Addr::new(10, 0, 1, last), 6881)
}

#[tokio::test]
async fn multi_hop_walk_advances_as_replies_arrive() {
    // Four hops, each answering promptly with one node closer to the target and the last
    // handing over a peer. The first peer must arrive at network speed, not after one
    // fixed response window per hop.
    let target = [0u8; 20];
    let hops: Vec<UdpSocket> = bind_loopback(4).await;
    let addrs: Vec<SocketAddrV4> = hops.iter().map(v4_addr).collect();
    let peer: SocketAddrV4 = "10.0.0.1:1111".parse().unwrap();
    let mut servers = Vec::new();
    for (i, hop) in hops.into_iter().enumerate() {
        let next = addrs.get(i + 1).copied();
        servers.push(answer_once(hop, move |txid| match next {
            Some(next) => {
                let mut id = [0u8; 20];
                id[0] = 0x80 >> i;
                response_with(txid, &[], &[(id, next)])
            }
            None => response_with(txid, &[peer], &[]),
        }));
    }
    let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let start = Instant::now();
    let mut first_peer = None;
    let metrics = dht_walk_from_seeds(
        &target,
        Duration::from_secs(10),
        &client,
        vec![addrs[0]],
        |_src, resp| {
            if resp.peers.contains(&peer) {
                first_peer = Some(start.elapsed());
            }
            first_peer.is_some()
        },
    )
    .await;
    for server in servers {
        server.abort();
    }
    let first_peer = first_peer.expect("chain end never reached");
    eprintln!("multi-hop first peer after {first_peer:?}");
    assert_eq!(metrics.nodes_queried, 4);
    assert_eq!(metrics.valid_responses, 4);
    assert_eq!(metrics.recovery_queries, 0);
    assert!(
        first_peer < Duration::from_millis(300),
        "4-hop walk took {first_peer:?}; it is waiting out fixed rounds"
    );
}

#[tokio::test]
async fn correlated_reply_frees_a_query_slot_immediately() {
    // Nine seeds in frontier order: the first answers at once (with nothing useful), the
    // next seven stay silent, and the ninth is only reachable once a slot frees. The
    // answer must free that slot right away rather than at the end of a round.
    let sockets = bind_loopback(9).await;
    let seeds: Vec<SocketAddrV4> = sockets.iter().map(v4_addr).collect();
    let mut sockets = sockets.into_iter();
    let fast = answer_once(sockets.next().unwrap(), |txid| {
        response_with(txid, &[], &[])
    });
    let silent: Vec<UdpSocket> = sockets.by_ref().take(7).collect();
    let peer: SocketAddrV4 = "10.0.0.2:2222".parse().unwrap();
    let ninth = answer_once(sockets.next().unwrap(), move |txid| {
        response_with(txid, &[peer], &[])
    });
    let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let start = Instant::now();
    let mut found = None;
    dht_walk_from_seeds(
        &[0u8; 20],
        Duration::from_secs(5),
        &client,
        seeds,
        |_src, resp| {
            if resp.peers.contains(&peer) {
                found = Some(start.elapsed());
            }
            found.is_some()
        },
    )
    .await;
    fast.abort();
    ninth.abort();
    drop(silent);
    let found = found.expect("ninth seed never queried");
    assert!(
        found < Duration::from_millis(300),
        "freed slot was not refilled until {found:?}"
    );
}

#[tokio::test]
async fn silent_queries_release_their_slots_after_the_soft_timeout() {
    // Eight silent seeds fill every slot. The ninth must not be queried before the soft
    // slot timeout, and must not wait for the longer correlation TTL either.
    let sockets = bind_loopback(9).await;
    let seeds: Vec<SocketAddrV4> = sockets.iter().map(v4_addr).collect();
    let mut sockets = sockets.into_iter();
    let silent: Vec<UdpSocket> = sockets.by_ref().take(8).collect();
    let peer: SocketAddrV4 = "10.0.0.3:3333".parse().unwrap();
    let ninth = answer_once(sockets.next().unwrap(), move |txid| {
        response_with(txid, &[peer], &[])
    });
    let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let start = Instant::now();
    let metrics = dht_walk_from_seeds(
        &[0u8; 20],
        Duration::from_secs(5),
        &client,
        seeds,
        |_src, resp| resp.peers.contains(&peer),
    )
    .await;
    let mut buf = [0u8; 2048];
    for socket in &silent {
        assert!(
            socket.try_recv_from(&mut buf).is_ok(),
            "silent seed unqueried"
        );
        assert!(
            socket.try_recv_from(&mut buf).is_err(),
            "silent seed queried twice"
        );
    }
    // The walk stops on the ninth seed's reply, so its length is when that seed was asked.
    let elapsed = start.elapsed();
    ninth.abort();
    assert_eq!(metrics.nodes_queried, 9);
    assert_eq!(metrics.valid_responses, 1);
    assert_eq!(metrics.timeouts, 8);
    assert!(
        elapsed >= SLOT_TIMEOUT - Duration::from_millis(100),
        "ninth seed queried before any slot released ({elapsed:?})"
    );
    assert!(
        elapsed < SLOT_TIMEOUT + Duration::from_millis(700),
        "slots were held for {elapsed:?}"
    );
}

#[tokio::test]
async fn reply_after_inflight_ttl_is_rejected_without_an_intervening_wake() {
    // The only query's slot releases at SLOT_TIMEOUT; a still-pending bootstrap lookup then
    // keeps the walk parked with no other wake before the reply lands past INFLIGHT_TTL.
    // That late packet must not be correlated against the expired entry.
    let server = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let seed = v4_addr(&server);
    let peer: SocketAddrV4 = "10.0.0.4:4444".parse().unwrap();
    let replier = tokio::spawn(async move {
        let mut buf = [0u8; 2048];
        let (n, caller) = server.recv_from(&mut buf).await.unwrap();
        tokio::time::sleep(INFLIGHT_TTL + Duration::from_millis(200)).await;
        let resp = response_with(&buf_txid(&buf[..n]), &[peer], &[]);
        server.send_to(&resp, caller).await.unwrap();
        std::future::pending::<()>().await
    });
    let mut lookups = tokio::task::JoinSet::new();
    lookups.spawn(async {
        std::future::pending::<()>().await;
        (0, Vec::new())
    });
    let mut frontier = BTreeMap::new();
    frontier.insert([0xff; 20], seed);
    let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let mut accepted = false;
    let metrics = dht_walk_owned(
        &[0u8; 20],
        Instant::now() + INFLIGHT_TTL + Duration::from_millis(600),
        &client,
        frontier,
        Vec::new(),
        lookups,
        None,
        |_src, resp| {
            accepted |= resp.peers.contains(&peer);
            false
        },
    )
    .await;
    replier.abort();
    assert!(!accepted, "reply past INFLIGHT_TTL was correlated");
    assert_eq!(metrics.valid_responses, 0);
    assert_eq!(metrics.uncorrelated_responses, 1);
}

#[tokio::test]
async fn correlated_krpc_error_frees_its_slot_immediately() {
    // A node that answers with a KRPC error for our transaction will not send a usable
    // reply, so its slot must free at once rather than at SLOT_TIMEOUT.
    let server = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let seed = v4_addr(&server);
    let handle = answer_once(server, error_response);
    let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let start = Instant::now();
    let metrics = dht_walk_from_seeds(
        &[9u8; 20],
        Duration::from_secs(10),
        &client,
        vec![seed],
        |_src, _resp| false,
    )
    .await;
    let elapsed = start.elapsed();
    handle.abort();
    assert_eq!(metrics.krpc_errors, 1);
    assert_eq!(metrics.timeouts, 0);
    assert!(metrics.frontier_exhausted);
    assert!(
        elapsed < Duration::from_millis(500),
        "waited {elapsed:?} on a node that already refused"
    );
}

#[test]
fn krpc_error_txid_requires_a_code_and_message_payload() {
    assert_eq!(
        krpc_error_txid(&error_response(b"ab")),
        Some(b"ab".to_vec())
    );
    let invalid = [
        None,
        Some(Bencode::Bytes(b"err".to_vec())),
        Some(Bencode::List(vec![])),
        Some(Bencode::List(vec![Bencode::Int(201)])),
        Some(Bencode::List(vec![
            Bencode::Bytes(b"201".to_vec()),
            Bencode::Bytes(b"err".to_vec()),
        ])),
        Some(Bencode::List(vec![
            Bencode::Int(201),
            Bencode::Bytes(b"err".to_vec()),
            Bencode::Int(1),
        ])),
    ];
    for e in invalid {
        let label = format!("{e:?}");
        assert_eq!(
            krpc_error_txid(&error_with_payload(b"ab", e)),
            None,
            "{label}"
        );
    }
}

#[tokio::test]
async fn correlated_error_without_a_valid_payload_does_not_close_the_query() {
    // Errors echoing the right transaction but with a missing or invalid `e` are
    // malformed, so they must leave the query open for the genuine reply that follows.
    let server = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let seed = v4_addr(&server);
    let peer: SocketAddrV4 = "10.0.0.7:7777".parse().unwrap();
    let handle = tokio::spawn(async move {
        let mut buf = [0u8; 2048];
        let (n, caller) = server.recv_from(&mut buf).await.unwrap();
        let txid = buf_txid(&buf[..n]);
        for e in [None, Some(Bencode::Bytes(b"err".to_vec()))] {
            server
                .send_to(&error_with_payload(&txid, e), caller)
                .await
                .unwrap();
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
        server
            .send_to(&response_with(&txid, &[peer], &[]), caller)
            .await
            .unwrap();
        std::future::pending::<()>().await
    });
    let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let mut found = false;
    let metrics = dht_walk_from_seeds(
        &[9u8; 20],
        Duration::from_secs(5),
        &client,
        vec![seed],
        |_src, resp| {
            found |= resp.peers.contains(&peer);
            found
        },
    )
    .await;
    handle.abort();
    assert!(found, "malformed error closed the query: {metrics:?}");
    assert_eq!(metrics.malformed_responses, 2);
    assert_eq!(metrics.krpc_errors, 0);
    assert_eq!(metrics.valid_responses, 1);
}

#[tokio::test]
async fn uncorrelated_krpc_error_does_not_close_the_query() {
    // A KRPC error with the wrong transaction id may be spoofed; it must not close the
    // query, so a genuine reply that follows is still accepted.
    let server = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let seed = v4_addr(&server);
    let peer: SocketAddrV4 = "10.0.0.6:6666".parse().unwrap();
    let handle = tokio::spawn(async move {
        let mut buf = [0u8; 2048];
        let (n, caller) = server.recv_from(&mut buf).await.unwrap();
        let txid = buf_txid(&buf[..n]);
        let mut wrong = txid.clone();
        wrong.push(0xff);
        server
            .send_to(&error_response(&wrong), caller)
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;
        server
            .send_to(&response_with(&txid, &[peer], &[]), caller)
            .await
            .unwrap();
        std::future::pending::<()>().await
    });
    let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let mut found = false;
    let metrics = dht_walk_from_seeds(
        &[9u8; 20],
        Duration::from_secs(5),
        &client,
        vec![seed],
        |_src, resp| {
            found |= resp.peers.contains(&peer);
            found
        },
    )
    .await;
    handle.abort();
    assert!(found, "spoofable error closed the query: {metrics:?}");
    assert_eq!(metrics.krpc_errors, 1);
    assert_eq!(metrics.valid_responses, 1);
}

#[tokio::test]
async fn bootstrap_resolution_keeps_distinct_ipv4_addresses_up_to_the_cap() {
    let resolver = Resolver::controlled(|_| {
        Ok(vec![
            "[::1]:6881".parse().unwrap(),
            "10.0.0.1:6881".parse().unwrap(),
            "10.0.0.1:6881".parse().unwrap(),
            "10.0.0.2:6881".parse().unwrap(),
            "10.0.0.3:6881".parse().unwrap(),
        ])
    });
    let deadline = tokio::time::Instant::now() + Duration::from_secs(1);
    let addrs = resolve_bootstrap("router.invalid:6881".into(), resolver.clone(), deadline).await;
    resolver.finish_controlled().await;
    let expected: Vec<SocketAddrV4> = vec![
        "10.0.0.1:6881".parse().unwrap(),
        "10.0.0.2:6881".parse().unwrap(),
    ];
    assert_eq!(addrs, expected);
}

#[tokio::test]
async fn every_address_behind_a_bootstrap_host_is_seeded() {
    // A router behind two addresses: the first drops every query, the second answers.
    // Asking only the first address would lose the router outright.
    let dead = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let live = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let addrs = vec![
        SocketAddr::V4(v4_addr(&dead)),
        SocketAddr::V4(v4_addr(&live)),
    ];
    let peer: SocketAddrV4 = "10.0.0.9:9999".parse().unwrap();
    let server = answer_once(live, move |txid| response_with(txid, &[peer], &[]));
    let resolver = Resolver::controlled(move |_| Ok(addrs.clone()));
    let socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let mut found = false;
    let metrics = dht_walk_with_bootstraps(
        &[0; 20],
        Duration::from_secs(2),
        &socket,
        &["router.invalid:1"],
        resolver.clone(),
        None,
        |_, response| {
            found |= response.peers.contains(&peer);
            found
        },
    )
    .await;
    server.abort();
    resolver.finish_controlled().await;
    drop(dead);
    assert!(
        found,
        "second address behind the bootstrap host was never asked"
    );
    assert_eq!(metrics.bootstrap_seeded, 2);
    assert_eq!(metrics.recovery_queries, 0);
}

#[tokio::test]
async fn dead_end_walk_recovers_by_widening_at_an_answering_router() {
    // The failure from #494: the router answers, but its only referral is silent, so the
    // frontier runs dry after one hop. The recovery `find_node` gets a fresh referral
    // that does hold peers.
    let target = [0u8; 20];
    let sockets = bind_loopback(3).await;
    let addrs: Vec<SocketAddrV4> = sockets.iter().map(v4_addr).collect();
    let (router, silent_referral, good_referral) = (addrs[0], addrs[1], addrs[2]);
    let mut sockets = sockets.into_iter();
    let router_sock = sockets.next().unwrap();
    let silent_sock = sockets.next().unwrap();
    let good_sock = sockets.next().unwrap();
    let peer: SocketAddrV4 = "10.0.0.7:7777".parse().unwrap();
    let (router_task, asked) = scripted_router(router_sock, move |_, method, txid| {
        let referral = if method == b"find_node" {
            ([0x60; 20], good_referral)
        } else {
            ([0x40; 20], silent_referral)
        };
        Some(response_with(txid, &[], &[referral]))
    });
    let good_task = answer_once(good_sock, move |txid| response_with(txid, &[peer], &[]));

    let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let start = Instant::now();
    let mut found = false;
    let metrics = dht_walk_from_bootstrap_seeds(
        &target,
        Duration::from_secs(10),
        &client,
        vec![router],
        None,
        |_, response| {
            found |= response.peers.contains(&peer);
            found
        },
    )
    .await;
    let elapsed = start.elapsed();
    router_task.abort();
    good_task.abort();
    drop(silent_sock);
    assert!(
        found,
        "recovery round did not rescue the dead-end walk: {metrics:?}"
    );
    assert_eq!(
        *asked.lock().unwrap(),
        vec![b"get_peers".to_vec(), b"find_node".to_vec()]
    );
    assert_eq!(metrics.recovery_queries, 1);
    assert_eq!(metrics.recovery_peers, 1);
    assert_eq!(metrics.nodes_queried, 4);
    assert!(!metrics.frontier_exhausted);
    // One slot timeout for the silent referral, then network speed.
    assert!(
        elapsed < SLOT_TIMEOUT + Duration::from_millis(500),
        "took {elapsed:?}"
    );
}

#[tokio::test]
async fn silent_router_is_asked_once_more_with_a_fresh_transaction() {
    // A router that dropped the first query (loss, rate limiting) gets one fresh
    // `get_peers` and answers it.
    let router_sock = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let router = v4_addr(&router_sock);
    let peer: SocketAddrV4 = "10.0.0.8:8888".parse().unwrap();
    let rejected_peer: SocketAddrV4 = "127.0.0.1:18888".parse().unwrap();
    let mut router_task = tokio::spawn(async move {
        let mut bytes = [0; 2048];
        let (n, client) = router_sock.recv_from(&mut bytes).await.unwrap();
        assert_eq!(query_method(&bytes[..n]), b"get_peers");
        let old_txid = buf_txid(&bytes[..n]);
        let (n, retry_client) = router_sock.recv_from(&mut bytes).await.unwrap();
        assert_eq!(client, retry_client);
        assert_eq!(query_method(&bytes[..n]), b"get_peers");
        let fresh_txid = buf_txid(&bytes[..n]);
        assert_ne!(
            old_txid, fresh_txid,
            "recovery must reserve a fresh transaction"
        );
        router_sock
            .send_to(&response_with(&old_txid, &[rejected_peer], &[]), client)
            .await
            .unwrap();
        router_sock
            .send_to(&response_with(&fresh_txid, &[peer], &[]), client)
            .await
            .unwrap();
    });
    let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let mut found = false;
    let metrics = dht_walk_from_bootstrap_seeds(
        &[0; 20],
        Duration::from_secs(10),
        &client,
        vec![router],
        None,
        |_, response| {
            assert!(
                !response.peers.contains(&rejected_peer),
                "superseded transaction accepted"
            );
            found |= response.peers.contains(&peer);
            found
        },
    )
    .await;
    let finished = tokio::time::timeout(Duration::from_secs(1), &mut router_task).await;
    if finished.is_err() {
        router_task.abort();
    }
    finished.expect("router fixture did not finish").unwrap();
    assert!(
        found,
        "re-asked router's answer was not accepted: {metrics:?}"
    );
    assert_eq!(metrics.uncorrelated_responses, 1);
    assert_eq!(metrics.query_attempts, 2);
    assert_eq!(metrics.recovery_queries, 1);
    assert_eq!(metrics.timeouts, 1);
}

#[tokio::test]
async fn recovery_runs_once_and_a_truly_dead_walk_still_ends() {
    // Every router and referral silent: one recovery query per bootstrap, then the walk
    // ends exhausted well inside its budget instead of retrying forever.
    let sockets = bind_loopback(2).await;
    let seeds: Vec<SocketAddrV4> = sockets.iter().map(v4_addr).collect();
    let mut tasks = Vec::new();
    let mut logs = Vec::new();
    for sock in sockets {
        let (task, asked) = scripted_router(sock, |_, _, _| None);
        tasks.push(task);
        logs.push(asked);
    }
    let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let start = Instant::now();
    let metrics = dht_walk_from_bootstrap_seeds(
        &[0; 20],
        Duration::from_secs(10),
        &client,
        seeds,
        None,
        |_, _| false,
    )
    .await;
    let elapsed = start.elapsed();
    for task in tasks {
        task.abort();
    }
    for asked in logs {
        assert_eq!(
            asked.lock().unwrap().len(),
            2,
            "each router is asked exactly twice"
        );
    }
    assert_eq!(metrics.recovery_queries, 2);
    assert_eq!(metrics.nodes_queried, 4);
    assert_eq!(metrics.timeouts, 4);
    assert!(metrics.frontier_exhausted);
    assert!(
        elapsed < 2 * SLOT_TIMEOUT + Duration::from_millis(500),
        "took {elapsed:?}"
    );
}

#[tokio::test]
async fn spoofed_traffic_cannot_mark_a_router_answered_or_add_queries() {
    // A spoofer floods the client with well-formed responses naming the router's txid
    // space and a referral of its own. None correlates, so the router still counts as
    // unanswered (re-asked with `get_peers`, not widened), and the spoofed referral is
    // never queried.
    let router_sock = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let router = v4_addr(&router_sock);
    let lure = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let lure_addr = v4_addr(&lure);
    let lure_log = scripted_router(lure, |_, _, _| None);
    let (router_task, asked) = scripted_router(router_sock, |_, _, _| None);
    let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let client_addr = v4_addr(&client);
    let spoofer = tokio::spawn(async move {
        let sock = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        for i in 0u16..64 {
            let resp = response_with(&i.to_be_bytes(), &[], &[([0x11; 20], lure_addr)]);
            let _ = sock.send_to(&resp, client_addr).await;
            tokio::time::sleep(Duration::from_millis(40)).await;
        }
    });
    let metrics = dht_walk_from_bootstrap_seeds(
        &[0; 20],
        Duration::from_secs(10),
        &client,
        vec![router],
        None,
        |_, _| false,
    )
    .await;
    spoofer.abort();
    router_task.abort();
    lure_log.0.abort();
    assert_eq!(
        *asked.lock().unwrap(),
        vec![b"get_peers".to_vec(), b"get_peers".to_vec()]
    );
    assert!(
        lure_log.1.lock().unwrap().is_empty(),
        "spoofed referral was queried"
    );
    assert_eq!(metrics.valid_responses, 0);
    assert_eq!(metrics.nodes_queried, 2);
    assert_eq!(metrics.recovery_queries, 1);
    assert!(metrics.uncorrelated_responses > 0);
}

#[tokio::test]
async fn walk_that_runs_dry_holding_peers_skips_the_recovery_round() {
    // A sparse swarm: the router hands over one peer (below the target) and nothing
    // else. The walk already has a result, so it ends at once instead of asking again.
    let router_sock = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let router = v4_addr(&router_sock);
    let peer: SocketAddrV4 = "10.0.0.6:6666".parse().unwrap();
    let (router_task, asked) = scripted_router(router_sock, move |_, _, txid| {
        Some(response_with(txid, &[peer], &[]))
    });
    let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let start = Instant::now();
    let metrics = dht_walk_from_bootstrap_seeds(
        &[0; 20],
        Duration::from_secs(10),
        &client,
        vec![router],
        None,
        |_, _| false,
    )
    .await;
    let elapsed = start.elapsed();
    router_task.abort();
    assert_eq!(asked.lock().unwrap().len(), 1);
    assert_eq!(metrics.peers_discovered, 1);
    assert_eq!(metrics.recovery_queries, 0);
    assert!(metrics.frontier_exhausted);
    assert!(elapsed < Duration::from_millis(300), "took {elapsed:?}");
}

#[tokio::test]
async fn walk_that_reaches_its_deadline_sends_no_recovery_round() {
    // The silent router's slot outlives the budget, so the walk ends on its deadline and
    // never runs dry inside it: nothing more may be sent.
    let router_sock = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let router = v4_addr(&router_sock);
    let (router_task, asked) = scripted_router(router_sock, |_, _, _| None);
    let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let metrics = dht_walk_from_bootstrap_seeds(
        &[0; 20],
        SLOT_TIMEOUT - Duration::from_millis(500),
        &client,
        vec![router],
        None,
        |_, _| false,
    )
    .await;
    router_task.abort();
    assert_eq!(asked.lock().unwrap().len(), 1);
    assert_eq!(metrics.recovery_queries, 0);
    assert!(!metrics.frontier_exhausted);
}

#[test]
fn node_cache_keeps_newest_first_without_duplicates_up_to_the_cap() {
    let cache = NodeCache::new();
    let now = Instant::now();
    cache.record(&[addr(1), addr(2)], now);
    cache.record(&[addr(3), addr(1)], now);
    assert_eq!(
        cache.fresh(now, usize::MAX, |_| false),
        vec![addr(3), addr(1), addr(2)]
    );
    let many: Vec<SocketAddrV4> = (0..=NODE_CACHE_CAP as u8 + 4).map(addr).collect();
    cache.record(&many, now);
    let kept = cache.fresh(now, usize::MAX, |_| false);
    assert_eq!(kept.len(), NODE_CACHE_CAP);
    assert_eq!(kept[0], addr(0));
    assert_eq!(
        cache.fresh(now, 2, |a| *a == addr(0)),
        vec![addr(1), addr(2)]
    );
}

#[test]
fn node_cache_drops_entries_older_than_the_max_age() {
    let cache = NodeCache::new();
    let then = Instant::now();
    cache.record(&[addr(1)], then);
    cache.record(&[addr(2)], then + Duration::from_secs(60));
    let later = then + NODE_CACHE_MAX_AGE + Duration::from_secs(1);
    assert_eq!(cache.fresh(later, usize::MAX, |_| false), vec![addr(2)]);
    assert_eq!(cache.nodes.lock().unwrap().len(), 1, "expired entry kept");
}

#[test]
fn cached_age_counts_from_the_reply_not_the_end_of_the_walk() {
    // A node that answered at the start of a 20 s walk expires 15 minutes after that
    // reply, not 15 minutes after the walk ended.
    let start = Instant::now();
    let mut answered = Answered::default();
    answered.note(addr(1), true, start);
    answered.note(addr(2), false, start + Duration::from_secs(20));
    let cache = NodeCache::new();
    cache.record_answered(&answered.into_preferred_first(|_| false));
    let check = start + NODE_CACHE_MAX_AGE + Duration::from_secs(10);
    assert_eq!(cache.fresh(check, usize::MAX, |_| false), vec![addr(2)]);
    // A reply recorded at once is still eligible.
    let cache = NodeCache::new();
    cache.record_answered(&[(addr(3), start)]);
    assert_eq!(cache.fresh(start, usize::MAX, |_| false), vec![addr(3)]);
}

#[test]
fn an_older_reply_never_replaces_a_newer_one_in_the_cache() {
    let start = Instant::now();
    let cache = NodeCache::new();
    cache.record_answered(&[(addr(1), start + Duration::from_secs(10))]);
    cache.record_answered(&[(addr(1), start)]);
    let check = start + NODE_CACHE_MAX_AGE + Duration::from_secs(5);
    assert_eq!(cache.fresh(check, usize::MAX, |_| false), vec![addr(1)]);
}

#[test]
fn cache_order_follows_reply_time_not_which_walk_finished_first() {
    // B replied at t=20 in a walk that finished first; A replied at t=10 in a walk that
    // finished later. With one free slot, recovery must ask B.
    let start = Instant::now();
    let cache = NodeCache::new();
    cache.record_answered(&[(addr(2), start + Duration::from_secs(20))]);
    cache.record_answered(&[(addr(1), start + Duration::from_secs(10))]);
    let now = start + Duration::from_secs(30);
    assert_eq!(cache.fresh(now, 1, |_| false), vec![addr(2)]);
    assert_eq!(
        cache.fresh(now, usize::MAX, |_| false),
        vec![addr(2), addr(1)]
    );
}

#[test]
fn router_that_answered_before_its_lookup_finished_counts_as_answered() {
    let early = addr(1);
    let mut responded = HashSet::new();
    responded.insert(early);
    let mut frontier = BTreeMap::new();
    let mut bootstraps = Vec::new();
    let mut metrics = DhtWalkMetrics::default();
    add_bootstrap(
        Ok((0, vec![early, addr(2)])),
        &mut frontier,
        &mut bootstraps,
        &responded,
        &mut metrics,
    );
    let answered: Vec<bool> = bootstraps.iter().map(|node| node.answered).collect();
    assert_eq!(answered, vec![true, false]);
    assert_eq!(
        recovery_targets(&bootstraps, Vec::new())[0],
        (early, RecoveryQuery::FindNode)
    );
}

#[test]
fn a_filtered_router_does_not_cost_a_responder_its_cache_place() {
    // A router noted first, then eight real responders: once the router is filtered
    // out, all eight responders still fit the per-walk limit.
    let router = addr(200);
    let mut answered = Answered::default();
    answered.note(router, true, Instant::now());
    for i in 0..NODE_CACHE_PER_WALK as u8 {
        answered.note(addr(i), true, Instant::now());
    }
    let out = answered.into_preferred_first(|a| *a == router);
    let expected: Vec<SocketAddrV4> = (0..NODE_CACHE_PER_WALK as u8).map(addr).collect();
    assert_eq!(out.iter().map(|(a, _)| *a).collect::<Vec<_>>(), expected);
}

#[test]
fn answered_prefers_nodes_that_returned_peers_and_is_bounded() {
    let mut answered = Answered::default();
    for i in 0..20 {
        answered.note(addr(i), i % 2 == 1, Instant::now());
    }
    answered.note(addr(1), true, Instant::now());
    let out = answered.into_preferred_first(|_| false);
    assert_eq!(out.len(), NODE_CACHE_PER_WALK);
    assert!(
        out.iter().all(|(a, _)| a.ip().octets()[3] % 2 == 1),
        "{out:?}"
    );
}

#[test]
fn recovery_targets_keep_every_router_and_fill_only_free_slots_from_the_cache() {
    let routers: Vec<BootstrapNode> = (0..5)
        .map(|i| BootstrapNode {
            addr: addr(100 + i),
            answered: i == 3,
        })
        .collect();
    let cached: Vec<SocketAddrV4> = (0..8).map(addr).collect();
    // Five routers, as a real walk seeds: all five keep their #494 query and order, and
    // the cache gets the three free slots, newest first.
    let targets = recovery_targets(&routers, cached.clone());
    let expected: Vec<(SocketAddrV4, RecoveryQuery)> = routers
        .iter()
        .map(|node| {
            let query = if node.answered {
                RecoveryQuery::FindNode
            } else {
                RecoveryQuery::GetPeers
            };
            (node.addr, query)
        })
        .chain(cached[..3].iter().map(|a| (*a, RecoveryQuery::GetPeers)))
        .collect();
    assert_eq!(targets, expected);
    // Routers fill every slot: the cache gets none, so silent routers are never cut.
    let full: Vec<BootstrapNode> = (0..MAX_ACTIVE as u8)
        .map(|i| BootstrapNode {
            addr: addr(100 + i),
            answered: false,
        })
        .collect();
    let targets = recovery_targets(&full, cached.clone());
    assert_eq!(targets.len(), MAX_ACTIVE);
    assert!(targets.iter().all(|(a, _)| !cached.contains(a)));
    // No routers resolved: the cache alone, still capped.
    assert_eq!(recovery_targets(&[], cached.clone()).len(), MAX_ACTIVE);
    // No cache: identical to the #494 round.
    assert_eq!(recovery_targets(&routers, Vec::new()).len(), 5);
}

#[tokio::test]
async fn silent_routers_recover_through_a_remembered_node() {
    // Routers that stay silent even when asked again, but a node that answered an
    // earlier walk is still up and points at the swarm.
    let router_sock = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let router = v4_addr(&router_sock);
    let (router_task, router_asked) = scripted_router(router_sock, |_, _, _| None);
    let remembered_sock = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let remembered = v4_addr(&remembered_sock);
    let peer: SocketAddrV4 = "10.0.0.5:5555".parse().unwrap();
    let (remembered_task, remembered_asked) =
        scripted_router(remembered_sock, move |_, _, txid| {
            Some(response_with(txid, &[peer], &[]))
        });
    let cache = NodeCache::new();
    cache.record(&[remembered], Instant::now());
    let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let start = Instant::now();
    let mut found = false;
    let metrics = dht_walk_from_bootstrap_seeds(
        &[0; 20],
        Duration::from_secs(10),
        &client,
        vec![router],
        Some(&cache),
        |_, response| {
            found |= response.peers.contains(&peer);
            found
        },
    )
    .await;
    let elapsed = start.elapsed();
    router_task.abort();
    remembered_task.abort();
    assert!(
        found,
        "remembered node did not rescue the walk: {metrics:?}"
    );
    assert_eq!(router_asked.lock().unwrap().len(), 2);
    assert_eq!(
        *remembered_asked.lock().unwrap(),
        vec![b"get_peers".to_vec()]
    );
    assert_eq!(metrics.recovery_queries, 2);
    assert_eq!(metrics.recovery_peers, 1);
    assert!(
        elapsed < SLOT_TIMEOUT + Duration::from_millis(500),
        "took {elapsed:?}"
    );
}

#[tokio::test]
async fn unresolvable_routers_recover_through_a_remembered_node() {
    // Every bootstrap hostname fails to resolve, so the walk starts with nothing to ask.
    // Before #522 it ended at once with no peers.
    let remembered_sock = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let remembered = v4_addr(&remembered_sock);
    let peer: SocketAddrV4 = "10.0.0.4:4444".parse().unwrap();
    let server = answer_once(remembered_sock, move |txid| {
        response_with(txid, &[peer], &[])
    });
    let cache = NodeCache::new();
    cache.record(&[remembered], Instant::now());
    let resolver = Resolver::controlled(|_| {
        Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "no such host",
        ))
    });
    let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let mut found = false;
    let metrics = dht_walk_with_bootstraps(
        &[0; 20],
        Duration::from_secs(2),
        &client,
        &["router.invalid:1"],
        resolver.clone(),
        Some(&cache),
        |_, response| {
            found |= response.peers.contains(&peer);
            found
        },
    )
    .await;
    server.abort();
    resolver.finish_controlled().await;
    assert!(found, "walk with no routers ignored the cache: {metrics:?}");
    assert_eq!(metrics.bootstrap_seeded, 0);
    assert_eq!(metrics.recovery_queries, 1);
}

#[tokio::test]
async fn router_resolved_after_answering_as_a_referral_is_not_cached() {
    // Router A refers B; B answers before B's own lookup finishes, then resolves as a
    // router. Neither router may take a cache slot.
    use std::sync::{Arc, Condvar, Mutex};
    let sockets = bind_loopback(2).await;
    let addrs: Vec<SocketAddrV4> = sockets.iter().map(v4_addr).collect();
    let (router_a, router_b) = (addrs[0], addrs[1]);
    let mut sockets = sockets.into_iter();
    let peer: SocketAddrV4 = "10.0.0.2:2222".parse().unwrap();
    let gate = Arc::new((Mutex::new(false), Condvar::new()));
    let resolver = Resolver::controlled({
        let gate = gate.clone();
        move |host| {
            if host.starts_with("late.") {
                let mut released = gate.0.lock().unwrap();
                while !*released {
                    released = gate.1.wait(released).unwrap();
                }
                Ok(vec![SocketAddr::V4(router_b)])
            } else {
                Ok(vec![SocketAddr::V4(router_a)])
            }
        }
    });
    let (a_task, _) = scripted_router(sockets.next().unwrap(), move |_, _, txid| {
        Some(response_with(txid, &[], &[([0x20; 20], router_b)]))
    });
    let b_task = answer_once(sockets.next().unwrap(), move |txid| {
        response_with(txid, &[peer], &[])
    });
    let release = gate.clone();
    let cache = NodeCache::new();
    let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let metrics = dht_walk_with_bootstraps(
        &[0; 20],
        Duration::from_secs(3),
        &client,
        &["first.invalid:1", "late.invalid:1"],
        resolver.clone(),
        Some(&cache),
        |_, response| {
            // B's reply has been accepted while B is still unresolved: only now let
            // its lookup finish.
            if response.peers.contains(&peer) {
                *release.0.lock().unwrap() = true;
                release.1.notify_all();
            }
            false
        },
    )
    .await;
    *gate.0.lock().unwrap() = true;
    gate.1.notify_all();
    a_task.abort();
    b_task.abort();
    resolver.finish_controlled().await;
    assert_eq!(metrics.peers_discovered, 1, "{metrics:?}");
    assert_eq!(metrics.bootstrap_seeded, 2, "B never resolved: {metrics:?}");
    assert!(
        cache
            .fresh(Instant::now(), usize::MAX, |_| false)
            .is_empty(),
        "a router took a cache slot"
    );
}

#[tokio::test]
async fn healthy_walk_never_asks_the_cache_but_teaches_it() {
    // The router hands over a referral that returns peers. The remembered node must not
    // be asked, and the referral that answered is remembered for later walks.
    let sockets = bind_loopback(3).await;
    let addrs: Vec<SocketAddrV4> = sockets.iter().map(v4_addr).collect();
    let (router, referral, remembered) = (addrs[0], addrs[1], addrs[2]);
    let mut sockets = sockets.into_iter();
    let peer: SocketAddrV4 = "10.0.0.3:3333".parse().unwrap();
    let (router_task, _) = scripted_router(sockets.next().unwrap(), move |_, _, txid| {
        Some(response_with(txid, &[], &[([0x20; 20], referral)]))
    });
    let (referral_task, _) = scripted_router(sockets.next().unwrap(), move |_, _, txid| {
        Some(response_with(txid, &[peer], &[]))
    });
    let (remembered_task, remembered_asked) =
        scripted_router(sockets.next().unwrap(), |_, _, _| None);
    let cache = NodeCache::new();
    cache.record(&[remembered], Instant::now());
    let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let metrics = dht_walk_from_bootstrap_seeds(
        &[0; 20],
        Duration::from_secs(5),
        &client,
        vec![router],
        Some(&cache),
        |_, response| response.peers.contains(&peer),
    )
    .await;
    router_task.abort();
    referral_task.abort();
    remembered_task.abort();
    assert_eq!(metrics.peers_discovered, 1);
    assert_eq!(metrics.recovery_queries, 0);
    assert!(remembered_asked.lock().unwrap().is_empty());
    assert_eq!(
        cache.fresh(Instant::now(), usize::MAX, |_| false),
        vec![referral, remembered],
        "the router is not cached, the answering referral is, newest first"
    );
}

#[tokio::test]
async fn spoofed_replies_never_enter_the_cache() {
    // A flood of well-formed but uncorrelated responses during a dead walk leaves the
    // cache empty: only a reply to a query we sent can add a node.
    let router_sock = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let router = v4_addr(&router_sock);
    let (router_task, _) = scripted_router(router_sock, |_, _, _| None);
    let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let client_addr = v4_addr(&client);
    let spoofer = tokio::spawn(async move {
        let sock = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        for i in 0u16..64 {
            let resp = response_with(&i.to_be_bytes(), &[addr(9)], &[]);
            let _ = sock.send_to(&resp, client_addr).await;
            tokio::time::sleep(Duration::from_millis(40)).await;
        }
    });
    let cache = NodeCache::new();
    let metrics = dht_walk_from_bootstrap_seeds(
        &[0; 20],
        Duration::from_secs(10),
        &client,
        vec![router],
        Some(&cache),
        |_, _| false,
    )
    .await;
    spoofer.abort();
    router_task.abort();
    assert!(metrics.uncorrelated_responses > 0);
    assert_eq!(metrics.valid_responses, 0);
    assert!(cache
        .fresh(Instant::now(), usize::MAX, |_| false)
        .is_empty());
}

#[tokio::test]
async fn remembered_node_already_asked_this_walk_is_not_asked_again() {
    // The router's referral is also in the cache. It answered without peers, so asking it
    // the same question in the recovery round would only repeat that answer.
    let sockets = bind_loopback(2).await;
    let addrs: Vec<SocketAddrV4> = sockets.iter().map(v4_addr).collect();
    let (router, referral) = (addrs[0], addrs[1]);
    let mut sockets = sockets.into_iter();
    let (router_task, _) = scripted_router(sockets.next().unwrap(), move |n, _, txid| {
        (n == 0).then(|| response_with(txid, &[], &[([0x20; 20], referral)]))
    });
    let (referral_task, referral_asked) = scripted_router(sockets.next().unwrap(), |_, _, txid| {
        Some(response_with(txid, &[], &[]))
    });
    let cache = NodeCache::new();
    cache.record(&[referral], Instant::now());
    let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let metrics = dht_walk_from_bootstrap_seeds(
        &[0; 20],
        Duration::from_secs(10),
        &client,
        vec![router],
        Some(&cache),
        |_, _| false,
    )
    .await;
    router_task.abort();
    referral_task.abort();
    assert_eq!(referral_asked.lock().unwrap().len(), 1);
    assert_eq!(
        metrics.recovery_queries, 1,
        "only the router is asked again"
    );
    assert!(metrics.frontier_exhausted);
}

#[tokio::test]
async fn early_peer_target_does_not_cache_a_router_with_pending_dns() {
    // Router B answers as a referral and satisfies the peer target while its own DNS
    // remains pending. Learning must not cache B or delay delivery to identify it.
    use std::sync::{Arc, Condvar, Mutex};
    let sockets = bind_loopback(2).await;
    let addrs: Vec<SocketAddrV4> = sockets.iter().map(v4_addr).collect();
    let (router_a, router_b) = (addrs[0], addrs[1]);
    let mut sockets = sockets.into_iter();
    let peer: SocketAddrV4 = "10.0.0.2:2222".parse().unwrap();
    let gate = Arc::new((Mutex::new(false), Condvar::new()));
    let resolver = Resolver::controlled({
        let gate = gate.clone();
        move |host| {
            if host.starts_with("late.") {
                let mut released = gate.0.lock().unwrap();
                while !*released {
                    released = gate.1.wait(released).unwrap();
                }
                Ok(vec![SocketAddr::V4(router_b)])
            } else {
                Ok(vec![SocketAddr::V4(router_a)])
            }
        }
    });
    let (a_task, _) = scripted_router(sockets.next().unwrap(), move |_, _, txid| {
        Some(response_with(txid, &[], &[([0x20; 20], router_b)]))
    });
    let b_task = answer_once(sockets.next().unwrap(), move |txid| {
        response_with(txid, &[peer], &[])
    });
    let cache = NodeCache::new();
    let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let metrics = dht_walk_with_bootstraps(
        &[0; 20],
        Duration::from_secs(3),
        &client,
        &["first.invalid:1", "late.invalid:1"],
        resolver.clone(),
        Some(&cache),
        |_, response| response.peers.contains(&peer),
    )
    .await;
    *gate.0.lock().unwrap() = true;
    gate.1.notify_all();
    a_task.abort();
    b_task.abort();
    resolver.finish_controlled().await;
    assert_eq!(metrics.peers_discovered, 1, "{metrics:?}");
    assert_eq!(
        metrics.bootstrap_seeded, 1,
        "early stop waited for DNS: {metrics:?}"
    );
    assert!(
        cache
            .fresh(Instant::now(), usize::MAX, |_| false)
            .is_empty(),
        "a router took a cache slot"
    );
}

#[test]
fn query_attempt_budget_never_reuses_a_transaction_and_counts_failed_attempts() {
    let mut budget = QueryBudget::default();
    let mut reserved = BTreeSet::new();
    for _ in 0..MAX_QUERY_ATTEMPTS {
        // Reservation occurs before the OS send, so dropping an attempt still consumes it.
        assert!(reserved.insert(budget.take().expect("attempt allowed")));
    }
    assert!(budget.exhausted());
    assert_eq!(reserved.len(), MAX_QUERY_ATTEMPTS);
    for _ in 0..3 {
        assert_eq!(budget.take(), None);
    }
    assert_eq!(budget.attempts, MAX_QUERY_ATTEMPTS);
}
