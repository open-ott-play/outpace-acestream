//! Bounded HTTP(S) tracker discovery (BEP 3 / BEP 23). The engine's peer transport is IPv4.
//! Untrusted URLs and returned peer addresses must be public; DNS is pinned for the request,
//! proxy environment variables and redirects are disabled, and no URL is included in errors.

use crate::discover::{MAX_TRACKERS, MAX_TRACKER_URL_LEN};
use ace_tracker::codec::{AnnounceEvent, TransferState};
use ace_wire::bencode::Bencode;
use std::collections::BTreeSet;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, SocketAddrV4};
use std::time::Duration;
use tokio::task::JoinSet;

const MAX_BODY_BYTES: usize = 64 * 1024;
const MAX_PEERS: usize = 200;
const CONCURRENCY: usize = 4;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(2);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(4);
const DISCOVERY_TIMEOUT: Duration = Duration::from_secs(5);

type Result<T> = std::result::Result<T, &'static str>;

#[derive(Clone, Copy)]
pub(crate) struct Announce {
    pub infohash: [u8; 20],
    pub peer_id: [u8; 20],
    pub port: u16,
    pub transfer: TransferState,
    pub event: AnnounceEvent,
}

/// Emit HTTP results as soon as each tracker answers. The receiver and five-second
/// collection deadline own all request tasks; neither can leave detached work behind.
pub(crate) async fn discover_incremental(
    trackers: &[String],
    request: Announce,
    output: tokio::sync::mpsc::Sender<SocketAddrV4>,
) {
    let mut seen_urls = BTreeSet::new();
    let mut urls = trackers.iter().take(MAX_TRACKERS).filter(|url| {
        url.len() <= MAX_TRACKER_URL_LEN
            && (url.starts_with("http://") || url.starts_with("https://"))
            && seen_urls.insert(url.as_str())
    });
    let mut jobs = JoinSet::new();
    let mut peers = BTreeSet::new();
    let deadline = tokio::time::Instant::now() + DISCOVERY_TIMEOUT;
    loop {
        while jobs.len() < CONCURRENCY {
            let Some(url) = urls.next() else { break };
            let url = url.clone();
            jobs.spawn(async move { announce(&url, request).await });
        }
        if jobs.is_empty() {
            break;
        }
        let joined = tokio::select! {
            _ = output.closed() => return,
            _ = tokio::time::sleep_until(deadline) => return,
            joined = jobs.join_next() => joined,
        };
        if let Some(Ok(Ok(found))) = joined {
            for peer in found {
                if !peers.insert(peer) {
                    continue;
                }
                tokio::select! {
                    _ = tokio::time::sleep_until(deadline) => return,
                    result = output.send(peer) => if result.is_err() { return; },
                }
                if peers.len() >= MAX_PEERS {
                    return;
                }
            }
        }
    }
}

async fn announce(value: &str, request: Announce) -> Result<Vec<SocketAddrV4>> {
    tokio::time::timeout(REQUEST_TIMEOUT, async {
        let url = announce_url(value, request)?;
        let host = url.host_str().ok_or("tracker has no host")?;
        let addr = resolve_public(
            host,
            url.port_or_known_default().ok_or("tracker has no port")?,
        )
        .await?;
        fetch_from(url.as_str(), host, addr).await
    })
    .await
    .map_err(|_| "HTTP tracker timeout")?
}

fn announce_url(value: &str, request: Announce) -> Result<reqwest::Url> {
    if value.len() > MAX_TRACKER_URL_LEN
        || value
            .bytes()
            .any(|b| b.is_ascii_control() || b.is_ascii_whitespace() || b == b'\\')
    {
        return Err("invalid tracker URL");
    }
    let mut url = reqwest::Url::parse(value).map_err(|_| "invalid tracker URL")?;
    if !matches!(url.scheme(), "http" | "https")
        || url.host_str().is_none()
        || !value.starts_with(&format!("{}://", url.scheme()))
        || value
            .split_once("://")
            .is_none_or(|(_, tail)| tail.starts_with('/'))
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
    {
        return Err("unsupported tracker URL");
    }
    // Existing passkeys/signatures are kept byte-for-byte. Reject competing announce fields
    // instead of appending ambiguous duplicate values that a tracker may interpret differently.
    const FIELDS: &[&str] = &[
        "info_hash",
        "peer_id",
        "port",
        "uploaded",
        "downloaded",
        "left",
        "compact",
        "numwant",
        "event",
        "ip",
    ];
    if url
        .query_pairs()
        .any(|(key, _)| FIELDS.contains(&key.as_ref()))
    {
        return Err("tracker URL already contains announce fields");
    }
    let escaped = |bytes: &[u8]| {
        bytes
            .iter()
            .map(|b| format!("%{b:02X}"))
            .collect::<String>()
    };
    let mut query = url
        .query()
        .filter(|s| !s.is_empty())
        .map(|s| format!("{s}&"))
        .unwrap_or_default();
    query.push_str(&format!("info_hash={}&peer_id={}&port={}&uploaded={}&downloaded={}&left={}&compact=1&numwant={MAX_PEERS}",
        escaped(&request.infohash), escaped(&request.peer_id), request.port,
        request.transfer.uploaded, request.transfer.downloaded, request.transfer.left));
    if let Some(event) = match request.event {
        AnnounceEvent::None => None,
        AnnounceEvent::Started => Some("started"),
        AnnounceEvent::Stopped => Some("stopped"),
        AnnounceEvent::Completed => Some("completed"),
    } {
        query.push_str(&format!("&event={event}"));
    }
    url.set_query(Some(&query));
    Ok(url)
}

/// Conservative public unicast filter. HTTP never honors the UDP-only local tracker opt-in:
/// an untrusted HTTP URL must not reach cloud metadata, a proxy, or a private network service.
fn is_public(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => {
            let [a, b, _, _] = ip.octets();
            !(ip.is_private()
                || ip.is_loopback()
                || ip.is_link_local()
                || ip.is_documentation()
                || ip.is_broadcast()
                || a == 0
                || a >= 224
                || (a == 100 && b & 0xc0 == 0x40)
                || (a == 198 && b & 0xfe == 18)
                || (a == 192 && b == 0)
                || ip.octets()[..3] == [192, 88, 99])
        }
        IpAddr::V6(ip) => {
            if let Some(v4) = ip.to_ipv4_mapped() {
                return is_public(IpAddr::V4(v4));
            }
            let s = ip.segments();
            s[0] & 0xe000 == 0x2000
                && s[0] != 0x2002
                && !(s[0] == 0x2001 && (s[1] <= 0x01ff || s[1] == 0x0db8))
                && !(s[0] == 0x3fff && s[1] <= 0x0fff)
        }
    }
}

async fn resolve_public(host: &str, port: u16) -> Result<SocketAddr> {
    let host = host.trim_start_matches('[').trim_end_matches(']');
    if let Ok(ip) = host.parse::<IpAddr>() {
        return is_public(ip)
            .then_some(SocketAddr::new(ip, port))
            .ok_or("blocked tracker address");
    }
    let addresses = tokio::net::lookup_host((host, port))
        .await
        .map_err(|_| "tracker DNS failed")?;
    addresses
        .into_iter()
        .find(|a| is_public(a.ip()))
        .ok_or("blocked tracker address")
}

/// All production callers validate the destination first; tests pin a synthetic hostname to
/// their own loopback fixture. A second DNS lookup cannot rebind the actual connection.
async fn fetch_from(url: &str, host: &str, addr: SocketAddr) -> Result<Vec<SocketAddrV4>> {
    let client = reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(CONNECT_TIMEOUT)
        .timeout(REQUEST_TIMEOUT)
        .resolve(host, addr)
        .build()
        .map_err(|_| "HTTP tracker client failed")?;
    let mut response = client
        .get(url)
        .header("Accept-Encoding", "identity")
        .header("User-Agent", concat!("outpace/", env!("CARGO_PKG_VERSION")))
        .send()
        .await
        .map_err(|_| "HTTP tracker request failed")?;
    if !response.status().is_success() {
        return Err("HTTP tracker status");
    }
    if response
        .content_length()
        .is_some_and(|n| n > MAX_BODY_BYTES as u64)
    {
        return Err("HTTP tracker body too large");
    }
    let mut body = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| "HTTP tracker body failed")?
    {
        if body.len() + chunk.len() > MAX_BODY_BYTES {
            return Err("HTTP tracker body too large");
        }
        body.extend_from_slice(&chunk);
    }
    response_peers(&body)
}

fn response_peers(body: &[u8]) -> Result<Vec<SocketAddrV4>> {
    if body.len() > MAX_BODY_BYTES {
        return Err("HTTP tracker body too large");
    }
    let value = Bencode::parse(body).map_err(|_| "malformed HTTP tracker response")?;
    if !matches!(value, Bencode::Dict(_)) || value.get(b"failure reason").is_some() {
        return Err("HTTP tracker rejected announce");
    }
    let mut peers = BTreeSet::new();
    let mut add = |ip: Ipv4Addr, port: u16| {
        if port != 0 && is_public(IpAddr::V4(ip)) {
            peers.insert(SocketAddrV4::new(ip, port));
        }
    };
    let compact = match value.get(b"peers") {
        Some(Bencode::Bytes(bytes)) => Some(bytes.clone()),
        // Observed AceStream tracker compatibility: compact bytes encoded as a bencode list
        // of single-byte strings. Accept exactly that shape, never mixed or multi-byte items.
        Some(Bencode::List(items))
            if items
                .iter()
                .all(|x| matches!(x, Bencode::Bytes(b) if b.len() == 1)) =>
        {
            Some(items.iter().map(|x| x.as_bytes().unwrap()[0]).collect())
        }
        Some(Bencode::List(items)) if items.iter().all(|x| matches!(x, Bencode::Dict(_))) => {
            for item in items.iter().take(MAX_PEERS) {
                let ip = item
                    .get(b"ip")
                    .and_then(Bencode::as_bytes)
                    .and_then(|b| std::str::from_utf8(b).ok())
                    .and_then(|s| s.parse::<Ipv4Addr>().ok());
                let port = item
                    .get(b"port")
                    .and_then(Bencode::as_int)
                    .and_then(|n| u16::try_from(n).ok());
                if let (Some(ip), Some(port)) = (ip, port) {
                    add(ip, port);
                }
            }
            None
        }
        // IPv6 peers cannot be dialed by the current engine. Ignore a peers6-only result.
        None if matches!(value.get(b"peers6"), Some(Bencode::Bytes(_))) => None,
        _ => return Err("malformed HTTP tracker peers"),
    };
    if let Some(compact) = compact {
        if !compact.len().is_multiple_of(6) {
            return Err("malformed compact tracker peers");
        }
        for bytes in compact.chunks_exact(6).take(MAX_PEERS) {
            add(
                Ipv4Addr::new(bytes[0], bytes[1], bytes[2], bytes[3]),
                u16::from_be_bytes([bytes[4], bytes[5]]),
            );
        }
    }
    Ok(peers.into_iter().take(MAX_PEERS).collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    fn request() -> Announce {
        Announce {
            infohash: [0; 20],
            peer_id: [0xff; 20],
            port: 8621,
            transfer: TransferState {
                uploaded: 7,
                downloaded: 9,
                left: 11,
            },
            event: AnnounceEvent::Started,
        }
    }

    fn reply(peers: Bencode) -> Vec<u8> {
        Bencode::Dict(BTreeMap::from([
            (b"interval".to_vec(), Bencode::Int(60)),
            (b"peers".to_vec(), peers),
        ]))
        .encode()
    }

    #[test]
    fn binary_query_is_not_utf8_or_form_encoded_and_preserves_passkey() {
        let mut request = request();
        request.infohash[..8].copy_from_slice(&[0, 32, 37, 38, 43, 47, 63, 255]);
        let url = announce_url(
            "https://tracker.example.test/announce?passkey=a%2fb%3D",
            request,
        )
        .unwrap();
        assert!(url
            .as_str()
            .contains("passkey=a%2fb%3D&info_hash=%00%20%25%26%2B%2F%3F%FF"));
        assert!(url
            .as_str()
            .contains(&format!("&peer_id={}", "%FF".repeat(20))));
        assert!(url.as_str().ends_with(
            "port=8621&uploaded=7&downloaded=9&left=11&compact=1&numwant=200&event=started"
        ));
        request.event = AnnounceEvent::None;
        assert!(
            !announce_url("http://tracker.example.test/announce", request)
                .unwrap()
                .as_str()
                .contains("event=")
        );
        request.event = AnnounceEvent::Completed;
        request.transfer.left = 0;
        let url = announce_url("http://tracker.example.test/announce", request).unwrap();
        assert!(url.as_str().contains("left=0&"));
        assert!(url.as_str().ends_with("event=completed"));
    }

    #[test]
    fn invalid_or_ambiguous_tracker_urls_fail_before_a_request() {
        for url in [
            "file:///etc/passwd",
            "ftp://tracker.example.test/x",
            "http:///example.test/x",
            "http:example.test/x",
            "https://user:secret@example.test/x",
            "http://example.test/a b",
            "http://example.test/a\rb",
            "http://example.test/a\\b",
            "http://example.test/x#fragment",
            "http://example.test/x?%69nfo_hash=bad",
            "http://example.test/x?peer_id=bad",
        ] {
            assert!(announce_url(url, request()).is_err(), "{url}");
        }
        assert!(announce_url(
            &format!("http://example.test/{}", "x".repeat(MAX_TRACKER_URL_LEN)),
            request()
        )
        .is_err());
    }

    #[test]
    fn public_address_guard_blocks_internal_special_and_transition_addresses() {
        for ip in [
            "0.1.2.3",
            "10.0.0.1",
            "100.64.0.1",
            "127.0.0.1",
            "169.254.169.254",
            "172.16.0.1",
            "192.168.0.1",
            "192.0.0.1",
            "192.0.2.1",
            "192.88.99.1",
            "198.18.0.1",
            "198.51.100.1",
            "203.0.113.1",
            "224.0.0.1",
            "240.0.0.1",
            "255.255.255.255",
            "::",
            "::1",
            "::ffff:127.0.0.1",
            "fc00::1",
            "fe80::1",
            "ff02::1",
            "64:ff9b::a00:1",
            "2001:db8::1",
            "2001::1",
            "2002:7f00:1::1",
            "3fff::1",
        ] {
            assert!(!is_public(ip.parse().unwrap()), "{ip}");
        }
        for ip in [
            "8.8.8.8",
            "9.9.9.9",
            "1.1.1.1",
            "2606:4700:4700::1111",
            "::ffff:8.8.8.8",
        ] {
            assert!(is_public(ip.parse().unwrap()), "{ip}");
        }
    }

    #[tokio::test]
    async fn announce_rejects_literal_loopback_without_contacting_it() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let url = format!("http://{}/announce", listener.local_addr().unwrap());
        assert_eq!(
            announce(&url, request()).await.unwrap_err(),
            "blocked tracker address"
        );
        assert_eq!(
            listener.accept().unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock
        );
        assert_eq!(
            resolve_public("[::1]", 80).await.unwrap_err(),
            "blocked tracker address"
        );
        assert_eq!(
            resolve_public("localhost", 80).await.unwrap_err(),
            "blocked tracker address"
        );
    }

    #[test]
    fn compact_and_observed_byte_list_peers_are_equivalent_and_filtered() {
        let bytes = vec![
            8, 8, 8, 8, 0x1a, 0xe1, 127, 0, 0, 1, 0x1a, 0xe1, 9, 9, 9, 9, 0, 0, 8, 8, 8, 8, 0x1a,
            0xe1,
        ];
        let expected = vec!["8.8.8.8:6881".parse().unwrap()];
        assert_eq!(
            response_peers(&reply(Bencode::Bytes(bytes.clone()))).unwrap(),
            expected
        );
        let list = Bencode::List(bytes.into_iter().map(|b| Bencode::Bytes(vec![b])).collect());
        assert_eq!(response_peers(&reply(list)).unwrap(), expected);
    }

    #[test]
    fn dictionary_peers_allow_only_numeric_public_ipv4_and_valid_ports() {
        let entries = [
            ("8.8.8.8", 6881),
            ("8.8.8.8", 6881),
            ("127.0.0.1", 6881),
            ("tracker.example.test", 6881),
            ("9.9.9.9", 0),
            ("9.9.9.9", 65536),
            ("9.9.9.9", -1),
        ];
        let peers = entries
            .into_iter()
            .map(|(ip, port)| {
                Bencode::Dict(BTreeMap::from([
                    (b"ip".to_vec(), Bencode::Bytes(ip.as_bytes().to_vec())),
                    (b"port".to_vec(), Bencode::Int(port)),
                ]))
            })
            .collect();
        assert_eq!(
            response_peers(&reply(Bencode::List(peers))).unwrap(),
            vec!["8.8.8.8:6881".parse().unwrap()]
        );
    }

    #[test]
    fn rejects_tracker_failure_and_malformed_peer_shapes() {
        assert!(response_peers(b"d14:failure reason6:deniede").is_err());
        for peers in [
            Bencode::Bytes(vec![8; 5]),
            Bencode::Int(1),
            Bencode::List(vec![Bencode::Bytes(vec![8, 8, 8, 8, 0x1a, 0xe1])]),
            Bencode::List(vec![Bencode::Bytes(vec![8]), Bencode::Int(8)]),
            Bencode::List(vec![
                Bencode::Bytes(vec![8]),
                Bencode::Dict(BTreeMap::new()),
            ]),
        ] {
            assert!(response_peers(&reply(peers)).is_err());
        }
        assert!(response_peers(b"not bencode").is_err());
        assert!(response_peers(&vec![b'x'; MAX_BODY_BYTES + 1]).is_err());
    }

    #[test]
    fn caps_returned_peers_and_ignores_unsupported_ipv6_peers() {
        let bytes: Vec<u8> = (0..MAX_PEERS + 1)
            .flat_map(|i| [8, 1, (i / 256) as u8, (i % 256) as u8, 0x1a, 0xe1])
            .collect();
        assert_eq!(
            response_peers(&reply(Bencode::Bytes(bytes))).unwrap().len(),
            MAX_PEERS
        );
        let body = Bencode::Dict(BTreeMap::from([(
            b"peers6".to_vec(),
            Bencode::Bytes(vec![0; 18]),
        )]))
        .encode();
        assert!(response_peers(&body).unwrap().is_empty());
    }

    async fn fixture(response: Vec<u8>) -> (SocketAddr, tokio::task::JoinHandle<String>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            while !request.ends_with(b"\r\n\r\n") && request.len() < 8192 {
                request.push(stream.read_u8().await.unwrap());
            }
            let _ = stream.write_all(&response).await;
            String::from_utf8(request).unwrap()
        });
        (addr, task)
    }

    #[tokio::test]
    async fn fetch_uses_pinned_address_and_preserves_binary_query_on_the_wire() {
        let body = reply(Bencode::Bytes(vec![8, 8, 8, 8, 0x1a, 0xe1]));
        let response = [
            format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n", body.len()).into_bytes(),
            body,
        ]
        .concat();
        let (addr, served) = fixture(response).await;
        let url = announce_url(
            &format!(
                "http://tracker.example.test:{}/announce?passkey=a%2Fb",
                addr.port()
            ),
            request(),
        )
        .unwrap();
        assert_eq!(
            fetch_from(url.as_str(), "tracker.example.test", addr)
                .await
                .unwrap(),
            vec!["8.8.8.8:6881".parse().unwrap()]
        );
        let request = served.await.unwrap();
        assert!(request.starts_with("GET /announce?passkey=a%2Fb&info_hash=%00%00"));
        assert!(request
            .to_lowercase()
            .contains(&format!("host: tracker.example.test:{}", addr.port())));
    }

    #[tokio::test]
    async fn redirects_are_not_followed_even_to_another_reachable_socket() {
        let destination = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        destination.set_nonblocking(true).unwrap();
        let response = format!(
            "HTTP/1.1 302 Found\r\nLocation: http://{}/private\r\nContent-Length: 0\r\n\r\n",
            destination.local_addr().unwrap()
        )
        .into_bytes();
        let (addr, served) = fixture(response).await;
        let url = format!("http://tracker.example.test:{}/announce", addr.port());
        assert_eq!(
            fetch_from(&url, "tracker.example.test", addr)
                .await
                .unwrap_err(),
            "HTTP tracker status"
        );
        served.await.unwrap();
        assert_eq!(
            destination.accept().unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock
        );
    }

    #[tokio::test]
    async fn rejects_oversized_announced_and_streamed_bodies() {
        for response in [
            format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n",
                MAX_BODY_BYTES + 1
            )
            .into_bytes(),
            [
                b"HTTP/1.1 200 OK\r\nConnection: close\r\n\r\n".to_vec(),
                vec![b'x'; MAX_BODY_BYTES + 1],
            ]
            .concat(),
        ] {
            let (addr, served) = fixture(response).await;
            let url = format!("http://tracker.example.test:{}/announce", addr.port());
            assert_eq!(
                fetch_from(&url, "tracker.example.test", addr)
                    .await
                    .unwrap_err(),
                "HTTP tracker body too large"
            );
            served.await.unwrap();
        }
    }

    #[tokio::test]
    async fn stalled_body_obeys_total_request_deadline() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 10\r\n\r\n")
                .await
                .unwrap();
            std::future::pending::<()>().await;
        });
        let url = format!("http://tracker.example.test:{}/announce", addr.port());
        let result = tokio::time::timeout(
            REQUEST_TIMEOUT + Duration::from_secs(2),
            fetch_from(&url, "tracker.example.test", addr),
        )
        .await;
        task.abort();
        assert_eq!(
            result
                .expect("client did not enforce its deadline")
                .unwrap_err(),
            "HTTP tracker body failed"
        );
    }
}
