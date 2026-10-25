//! Bounded, independent acquisition of an exact historical peer-piece range.
//!
//! This is a preparation/probing API, not a UTC seek API or a decoder-ready archive.
//! A peer's window is only an availability claim. Source signatures authenticate media
//! bytes, but do **not** bind the external timestamp or the requested piece index.
//! Callers must establish media timing and a decoder random-access point separately.
//! In particular, a successful fetch alone does not prove that playback went backwards.
//!
//! Resolve a fresh, authenticated transport descriptor before calling this module:
//! live swarms rotate. No resolver cache, live follower, shared seed store or playback
//! cursor is accessed here. Dropping the returned future closes its single connection;
//! no detached tasks survive cancellation. Missing pieces never cause a jump to live.

use crate::types::StreamInfo;
use ace_peer::session::PeerSession;
use ace_wire::extended::{ExtendedHandshake, LivePosition, NodeFields, OutgoingExtendedHandshake};
use ace_wire::handshake::random_peer_id;
use ace_wire::identity::Identity;
use ace_wire::live_auth::signature_len_from_pubkey_der;
use ace_wire::live_codec::chunk_request;
use ace_wire::message::PeerMessage;
use ace_wire::reassembly::PieceReassembler;
use bytes::Bytes;
use std::collections::HashSet;
use std::fmt;
use std::io;
use std::net::SocketAddrV4;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::TcpStream;
use tokio::time::timeout;

const MAX_PIECES: u16 = 128;
const MAX_BYTES: u64 = 64 * 1024 * 1024;
const MAX_PIECE_BYTES: u64 = 8 * 1024 * 1024;
const MAX_CHUNKS: u64 = 512;
const MAX_MESSAGES: usize = 65_536;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HistoryRequest {
    pub start_piece: u32,
    pub piece_count: u16,
}

/// Limits cover the entire operation, including failed peers and handshake traffic.
/// Received wire bytes are capped at twice `max_bytes` plus 512 KiB of control traffic.
#[derive(Debug, Clone, Copy)]
pub struct HistoryLimits {
    pub total_timeout: Duration,
    pub connect_timeout: Duration,
    pub piece_timeout: Duration,
    pub max_peers: usize,
    pub max_bytes: u64,
}

impl Default for HistoryLimits {
    fn default() -> Self {
        Self {
            total_timeout: Duration::from_secs(30),
            connect_timeout: Duration::from_secs(3),
            piece_timeout: Duration::from_secs(5),
            max_peers: 8,
            max_bytes: MAX_BYTES,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct HistoryPiece {
    /// Echoed wire coordinate, not cryptographically bound to the media payload.
    pub index: u32,
    /// Source-signature-verified bytes, with the signature trailer removed.
    pub payload: Bytes,
    /// Unauthenticated advisory timestamp; never use as proof of media age.
    pub reported_unix_seconds: Option<f64>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct HistoryRange {
    /// Always the whole requested contiguous range; errors expose no partial result.
    pub pieces: Vec<HistoryPiece>,
    pub attempted_peers: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HistoryPeerWindow {
    pub peer: SocketAddrV4,
    pub min_piece: u32,
    pub max_piece: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HistoryFailure {
    NoPeers,
    Connect,
    Handshake,
    InvalidWindow,
    NotAdvertised,
    PeerIo,
    PeerTimeout,
    Choked,
    WrongStream,
    WrongPiece,
    MalformedChunk,
    ConflictingTimestamp,
    SignatureRejected,
    ReceiveLimit,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HistoryError {
    InvalidRequest(&'static str),
    VerificationRequired,
    Deadline,
    ReceiveLimit,
    Unavailable {
        piece: u32,
        attempted_peers: usize,
        last_failure: HistoryFailure,
    },
}

impl fmt::Display for HistoryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidRequest(reason) => write!(f, "invalid history request: {reason}"),
            Self::VerificationRequired => f.write_str("history requires a valid source verification key"),
            Self::Deadline => f.write_str("history acquisition deadline reached"),
            Self::ReceiveLimit => f.write_str("history receive budget exhausted"),
            Self::Unavailable { piece, attempted_peers, last_failure } => write!(
                f, "history piece {piece} unavailable after {attempted_peers} peers ({last_failure:?})"
            ),
        }
    }
}

impl std::error::Error for HistoryError {}

/// Fetch exactly `request`, using at most one independent connection at a time.
/// The descriptor must have been freshly resolved by the caller. Its verification
/// key and geometry are checked before any peer is contacted. Cancellation drops
/// all in-flight acquisition and does not affect an existing live session.
pub async fn fetch_history(
    info: &StreamInfo,
    identity: &Identity,
    peers: &[SocketAddrV4],
    request: HistoryRequest,
    limits: HistoryLimits,
) -> Result<HistoryRange, HistoryError> {
    validate(info, limits)?;
    if request.piece_count == 0 || request.piece_count > MAX_PIECES {
        return Err(HistoryError::InvalidRequest("piece count must be 1..=128"));
    }
    let end = u64::from(request.start_piece) + u64::from(request.piece_count);
    if end > u64::from(u32::MAX) + 1 {
        return Err(HistoryError::InvalidRequest("piece range overflows"));
    }
    if info.piece_length * u64::from(request.piece_count) > limits.max_bytes {
        return Err(HistoryError::InvalidRequest("range exceeds byte budget"));
    }
    timeout(
        limits.total_timeout,
        fetch_inner(info, identity, peers, request, end, limits),
    )
    .await
    .map_err(|_| HistoryError::Deadline)?
}

/// Inspect bounded, current peer availability claims without requesting media.
/// This does not guarantee the pieces exist or authenticate their advertised ages.
pub async fn inspect_history_windows(
    info: &StreamInfo,
    peers: &[SocketAddrV4],
    limits: HistoryLimits,
) -> Result<Vec<HistoryPeerWindow>, HistoryError> {
    validate(info, limits)?;
    timeout(limits.total_timeout, async {
        let mut windows = Vec::new();
        let mut budget = ReadBudget::new(limits);
        for peer in candidates(peers, limits.max_peers) {
            match timeout(
                limits.connect_timeout,
                connect_window(info, peer, &mut budget),
            )
            .await
            {
                Ok(Ok((_session, window))) => windows.push(window),
                Ok(Err(HistoryFailure::ReceiveLimit)) => return Err(HistoryError::ReceiveLimit),
                _ => {}
            }
        }
        Ok(windows)
    })
    .await
    .map_err(|_| HistoryError::Deadline)?
}

fn validate(info: &StreamInfo, limits: HistoryLimits) -> Result<(), HistoryError> {
    for (value, cap) in [
        (limits.total_timeout, Duration::from_secs(60)),
        (limits.connect_timeout, Duration::from_secs(10)),
        (limits.piece_timeout, Duration::from_secs(10)),
    ] {
        if value.is_zero() || value > cap {
            return Err(HistoryError::InvalidRequest(
                "timeout outside supported bounds",
            ));
        }
    }
    if !(1..=32).contains(&limits.max_peers) || !(1..=MAX_BYTES).contains(&limits.max_bytes) {
        return Err(HistoryError::InvalidRequest(
            "peer or byte limit outside supported bounds",
        ));
    }
    if info.piece_length == 0
        || info.piece_length > MAX_PIECE_BYTES
        || info.chunk_length == 0
        || info.chunk_length > 65_536
        || !info.piece_length.is_multiple_of(info.chunk_length)
        || info.piece_length / info.chunk_length > MAX_CHUNKS
    {
        return Err(HistoryError::InvalidRequest("unsupported piece geometry"));
    }
    if info.sig_len == 0
        || info.sig_len as u64 >= info.piece_length
        || info.source_pubkey.len() > 4096
        || signature_len_from_pubkey_der(&info.source_pubkey) != Some(info.sig_len)
    {
        return Err(HistoryError::VerificationRequired);
    }
    Ok(())
}

fn candidates(peers: &[SocketAddrV4], max: usize) -> Vec<SocketAddrV4> {
    let mut seen = HashSet::new();
    peers
        .iter()
        .copied()
        .filter(|peer| seen.insert(*peer))
        .take(max)
        .collect()
}

async fn fetch_inner(
    info: &StreamInfo,
    identity: &Identity,
    peers: &[SocketAddrV4],
    request: HistoryRequest,
    end: u64,
    limits: HistoryLimits,
) -> Result<HistoryRange, HistoryError> {
    let mut pieces = Vec::with_capacity(usize::from(request.piece_count));
    let mut next = u64::from(request.start_piece);
    let mut attempted = 0;
    let mut failure = HistoryFailure::NoPeers;
    let mut budget = ReadBudget::new(limits);
    for peer in candidates(peers, limits.max_peers) {
        attempted += 1;
        let connection = timeout(limits.connect_timeout, async {
            let (mut session, mut window) = connect_window(info, peer, &mut budget).await?;
            if next < u64::from(window.min_piece) || next > u64::from(window.max_piece) {
                return Err(HistoryFailure::NotAdvertised);
            }
            session
                .send_signed_extended_handshake(&outgoing(next as u32, window, peer), identity)
                .await
                .map_err(|_| HistoryFailure::PeerIo)?;
            session
                .send(&PeerMessage::Interested)
                .await
                .map_err(|_| HistoryFailure::PeerIo)?;
            for _ in 0..64 {
                match read(&mut session, &mut budget).await? {
                    PeerMessage::Unchoke => return Ok((session, window)),
                    PeerMessage::Extended { ext_id: 0, payload } => {
                        let updated = parse_window(&payload, peer)?;
                        if next < u64::from(updated.min_piece)
                            || next > u64::from(updated.max_piece)
                        {
                            return Err(HistoryFailure::NotAdvertised);
                        }
                        window = updated;
                    }
                    _ => {}
                }
            }
            Err(HistoryFailure::Choked)
        })
        .await;
        match connection {
            Ok(Ok((mut session, mut window))) => {
                while next < end {
                    if next < u64::from(window.min_piece) || next > u64::from(window.max_piece) {
                        failure = HistoryFailure::NotAdvertised;
                        break;
                    }
                    match timeout(
                        limits.piece_timeout,
                        fetch_piece(info, &mut session, next as u32, &mut window, &mut budget),
                    )
                    .await
                    {
                        Ok(Ok(piece)) => {
                            pieces.push(piece);
                            next += 1;
                        }
                        Ok(Err(reason)) => {
                            failure = reason;
                            break;
                        }
                        Err(_) => {
                            failure = HistoryFailure::PeerTimeout;
                            break;
                        }
                    }
                }
                if next == end {
                    return Ok(HistoryRange {
                        pieces,
                        attempted_peers: attempted,
                    });
                }
            }
            Ok(Err(reason)) => failure = reason,
            Err(_) => failure = HistoryFailure::PeerTimeout,
        }
        if failure == HistoryFailure::ReceiveLimit || budget.exhausted() {
            return Err(HistoryError::ReceiveLimit);
        }
    }
    Err(HistoryError::Unavailable {
        piece: next as u32,
        attempted_peers: attempted,
        last_failure: failure,
    })
}

fn outgoing(
    start: u32,
    window: HistoryPeerWindow,
    peer: SocketAddrV4,
) -> OutgoingExtendedHandshake {
    OutgoingExtendedHandshake {
        ace_metadata_version: 1,
        ut_metadata_id: 2,
        mi: Some(LivePosition {
            min_piece: i64::from(start),
            max_piece: i64::from(window.max_piece),
            // Match the regular follower bootstrap: we have not downloaded any pieces.
            position: -1,
            distance_from_source: -1,
        }),
        node: NodeFields {
            ts: 5000,
            ..NodeFields::default()
        },
        peer_ip: Some(peer.ip().octets()),
        metadata_size: None,
    }
}

async fn connect_window(
    info: &StreamInfo,
    peer: SocketAddrV4,
    budget: &mut ReadBudget,
) -> Result<(HistorySession, HistoryPeerWindow), HistoryFailure> {
    if budget.exhausted() {
        return Err(HistoryFailure::ReceiveLimit);
    }
    let stream = TcpStream::connect(peer)
        .await
        .map_err(|_| HistoryFailure::Connect)?;
    let mut session = PeerSession::new(BudgetedStream {
        inner: stream,
        remaining: Arc::clone(&budget.bytes),
    });
    session
        .perform_handshake(info.infohash, random_peer_id())
        .await
        .map_err(|_| budget.failure(HistoryFailure::Handshake))?;
    for _ in 0..32 {
        if let PeerMessage::Extended { ext_id: 0, payload } = read(&mut session, budget).await? {
            return Ok((session, parse_window(&payload, peer)?));
        }
    }
    Err(HistoryFailure::InvalidWindow)
}

fn parse_window(payload: &[u8], peer: SocketAddrV4) -> Result<HistoryPeerWindow, HistoryFailure> {
    let handshake = ExtendedHandshake::parse(payload).map_err(|_| HistoryFailure::InvalidWindow)?;
    let mi = handshake
        .raw
        .get(b"mi")
        .ok_or(HistoryFailure::InvalidWindow)?;
    let number = |name: &[u8]| {
        mi.get(name)
            .and_then(|n| n.as_int())
            .and_then(|n| u32::try_from(n).ok())
            .ok_or(HistoryFailure::InvalidWindow)
    };
    let min_piece = number(b"min_piece")?;
    let max_piece = number(b"max_piece")?;
    if min_piece > max_piece {
        return Err(HistoryFailure::InvalidWindow);
    }
    Ok(HistoryPeerWindow {
        peer,
        min_piece,
        max_piece,
    })
}

async fn fetch_piece(
    info: &StreamInfo,
    session: &mut HistorySession,
    piece: u32,
    window: &mut HistoryPeerWindow,
    budget: &mut ReadBudget,
) -> Result<HistoryPiece, HistoryFailure> {
    // Fresh per-piece state also prevents data from failed peers surviving fallback.
    let mut assembly = PieceReassembler::new(info.piece_length, u64::from(piece))
        .with_source_pubkey(info.source_pubkey.clone())
        .with_max_pieces_ahead(1);
    let chunks = info.piece_length / info.chunk_length;
    for chunk in 0..chunks {
        session
            .send(&chunk_request(piece, chunk as u16))
            .await
            .map_err(|_| HistoryFailure::PeerIo)?;
    }
    let mut header = None;
    loop {
        match read(session, budget).await? {
            PeerMessage::Piece {
                index,
                begin,
                block,
            } => {
                if index != 0 {
                    return Err(HistoryFailure::WrongStream);
                }
                if begin != piece {
                    return Err(HistoryFailure::WrongPiece);
                }
                if block.len() != info.chunk_length as usize + 10 {
                    return Err(HistoryFailure::MalformedChunk);
                }
                let chunk = u16::from_be_bytes([block[8], block[9]]);
                if u64::from(chunk) >= chunks {
                    return Err(HistoryFailure::MalformedChunk);
                }
                let value: [u8; 8] = block[..8].try_into().expect("chunk length checked");
                if header.is_some_and(|previous| previous != value) {
                    return Err(HistoryFailure::ConflictingTimestamp);
                }
                header = Some(value);
                assembly
                    .add_block(
                        u64::from(piece),
                        u64::from(chunk) * info.chunk_length,
                        &block[10..],
                    )
                    .map_err(|error| match error {
                        ace_wire::WireError::SignatureRejected => HistoryFailure::SignatureRejected,
                        _ => HistoryFailure::MalformedChunk,
                    })?;
                if assembly.has_complete_piece(u64::from(piece)) {
                    let timestamp = f64::from_be_bytes(value);
                    return Ok(HistoryPiece {
                        index: piece,
                        payload: Bytes::from(assembly.take_ready()),
                        reported_unix_seconds: (timestamp.is_finite() && timestamp > 0.0)
                            .then_some(timestamp),
                    });
                }
            }
            PeerMessage::Choke => return Err(HistoryFailure::Choked),
            PeerMessage::Extended { ext_id: 0, payload } => {
                *window = parse_window(&payload, window.peer)?;
                if piece < window.min_piece || piece > window.max_piece {
                    return Err(HistoryFailure::NotAdvertised);
                }
            }
            _ => {}
        }
    }
}

type HistorySession = PeerSession<BudgetedStream<TcpStream>>;

/// One independent peer connection reads at a time. Keeping the counter outside
/// the session charges handshake, read-ahead and partial frames even when that
/// session is dropped by a timeout or fallback. Atomic storage keeps the future
/// Send without holding a lock across I/O.
struct BudgetedStream<S> {
    inner: S,
    remaining: Arc<AtomicU64>,
}

impl<S: AsyncRead + Unpin> AsyncRead for BudgetedStream<S> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        if buf.remaining() == 0 {
            return Poll::Ready(Ok(()));
        }
        let this = self.get_mut();
        let available = this.remaining.load(Ordering::Relaxed);
        if available == 0 {
            return Poll::Ready(Err(io::Error::other("history receive budget exhausted")));
        }
        let limit = usize::try_from(available)
            .unwrap_or(usize::MAX)
            .min(buf.remaining());
        let mut limited = ReadBuf::new(buf.initialize_unfilled_to(limit));
        let result = Pin::new(&mut this.inner).poll_read(cx, &mut limited);
        let received = limited.filled().len();
        this.remaining.fetch_sub(received as u64, Ordering::Relaxed);
        buf.advance(received);
        result
    }
}

impl<S: AsyncWrite + Unpin> AsyncWrite for BudgetedStream<S> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.get_mut().inner).poll_write(cx, buf)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_shutdown(cx)
    }
}

struct ReadBudget {
    bytes: Arc<AtomicU64>,
    messages: usize,
}

impl ReadBudget {
    fn new(limits: HistoryLimits) -> Self {
        Self {
            bytes: Arc::new(AtomicU64::new(limits.max_bytes * 2 + 512 * 1024)),
            messages: MAX_MESSAGES,
        }
    }

    fn exhausted(&self) -> bool {
        self.bytes.load(Ordering::Relaxed) == 0
    }

    fn failure(&self, fallback: HistoryFailure) -> HistoryFailure {
        if self.exhausted() {
            HistoryFailure::ReceiveLimit
        } else {
            fallback
        }
    }

    fn consume_message(&mut self) -> Result<(), HistoryFailure> {
        self.messages = self
            .messages
            .checked_sub(1)
            .ok_or(HistoryFailure::ReceiveLimit)?;
        Ok(())
    }
}

async fn read(
    session: &mut HistorySession,
    budget: &mut ReadBudget,
) -> Result<PeerMessage, HistoryFailure> {
    if budget.messages == 0 {
        return Err(HistoryFailure::ReceiveLimit);
    }
    let msg = session
        .read_message()
        .await
        .map_err(|_| budget.failure(HistoryFailure::PeerIo))?;
    budget.consume_message()?;
    Ok(msg)
}

#[cfg(test)]
#[path = "history/tests.rs"]
mod tests;
