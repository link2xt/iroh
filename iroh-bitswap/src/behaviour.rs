//! Implements handling of
//! - `/ipfs/bitswap/1.1.0` and
//! - `/ipfs/bitswap/1.2.0`.

use std::collections::{HashMap, HashSet, VecDeque};
use std::task::{Context, Poll};
use std::time::Duration;

use ahash::AHashSet;
use bytes::Bytes;
use cid::Cid;
use iroh_metrics::inc;
use iroh_metrics::{bitswap::BitswapMetrics, core::MRecorder, record};
use libp2p::core::connection::ConnectionId;
use libp2p::core::{ConnectedPoint, Multiaddr, PeerId};
use libp2p::swarm::dial_opts::DialOpts;
use libp2p::swarm::handler::OneShotHandler;
use libp2p::swarm::{
    DialError, IntoConnectionHandler, NetworkBehaviour, NetworkBehaviourAction, NotifyHandler,
    OneShotHandlerConfig, PollParameters, SubstreamProtocol,
};
use tracing::{debug, instrument, trace, warn};

use crate::message::{BitswapMessage, BlockPresence, Priority};
use crate::protocol::{BitswapProtocol, Upgrade};
// use crate::session::{Config as SessionConfig, SessionManager};
use crate::Block;

const MAX_PROVIDERS: usize = 10; // yolo

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum BitswapEvent {
    OutboundQueryCompleted { result: QueryResult },
    InboundRequest { request: InboundRequest },
}

#[derive(Clone, Debug, Eq, PartialEq)]
#[allow(clippy::large_enum_variant)]
pub enum QueryResult {
    Want(WantResult),
    FindProviders(FindProvidersResult),
    Send(SendResult),
    SendHave(SendHaveResult),
    Cancel(CancelResult),
}

#[derive(Clone, Debug, Eq, PartialEq)]
#[allow(clippy::large_enum_variant)]
pub enum WantResult {
    Ok {
        sender: PeerId,
        cid: Cid,
        data: Bytes,
    },
    Err {
        cid: Cid,
        error: QueryError,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
#[allow(clippy::large_enum_variant)]
pub enum FindProvidersResult {
    Ok { cid: Cid, provider: PeerId },
    Err { cid: Cid, error: QueryError },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SendHaveResult {
    Ok(Cid),
    Err { cid: Cid, error: QueryError },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SendResult {
    Ok(Cid),
    Err { cid: Cid, error: QueryError },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CancelResult {
    Ok(Cid),
    Err { cid: Cid, error: QueryError },
}

#[derive(Debug, Clone, Eq, PartialEq, thiserror::Error)]
pub enum QueryError {
    #[error("timeout")]
    Timeout,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum InboundRequest {
    Want {
        sender: PeerId,
        cid: Cid,
        priority: Priority,
    },
    WantHave {
        sender: PeerId,
        cid: Cid,
        priority: Priority,
    },
    Cancel {
        sender: PeerId,
        cid: Cid,
    },
}

pub type BitswapHandler = OneShotHandler<BitswapProtocol, BitswapMessage, HandlerEvent>;

/// Network behaviour that handles sending and receiving IPFS blocks.
#[derive(Default)]
pub struct Bitswap {
    /// Queue of events to report to the user.
    events: VecDeque<NetworkBehaviourAction<BitswapEvent, BitswapHandler>>,
    #[allow(dead_code)]
    config: BitswapConfig,
    known_peers: HashMap<PeerId, PeerState>,
}

#[derive(Debug, Clone, PartialEq)]
struct PeerState {
    conn: ConnState,
    msg: BitswapMessage,
}

impl PeerState {
    fn is_connected(&self) -> bool {
        matches!(self.conn, ConnState::Connected)
    }

    fn needs_connection(&self) -> bool {
        !self.is_empty() && matches!(self.conn, ConnState::Disconnected | ConnState::Unknown)
    }

    fn is_empty(&self) -> bool {
        self.msg.is_empty()
    }

    fn send_message(&mut self) -> BitswapMessage {
        std::mem::take(&mut self.msg)
    }

    fn want_block(&mut self, cid: &Cid, priority: Priority) {
        self.msg.wantlist_mut().want_block(cid, priority);
    }

    fn cancel_block(&mut self, cid: &Cid) {
        self.msg.wantlist_mut().cancel_block(cid);
    }

    fn remove_block(&mut self, cid: &Cid) {
        self.msg.wantlist_mut().remove_block(cid);
    }

    fn send_block(&mut self, cid: Cid, data: Bytes) {
        self.msg.add_block(Block { cid, data });
    }

    fn want_have_block(&mut self, cid: &Cid, priority: Priority) {
        self.msg.wantlist_mut().want_have_block(cid, priority);
    }

    fn remove_want_block(&mut self, cid: &Cid) {
        self.msg.wantlist_mut().remove_want_block(cid);
    }

    fn send_have_block(&mut self, cid: Cid) {
        self.msg.add_block_presence(BlockPresence::have(cid));
    }
}

impl Default for PeerState {
    fn default() -> Self {
        PeerState {
            conn: ConnState::Unknown,
            msg: Default::default(),
        }
    }
}

#[derive(Debug, Copy, Clone, PartialEq)]
enum ConnState {
    Unknown,
    Connected,
    Disconnected,
    Dialing,
}

#[derive(Default, Debug, Clone, PartialEq, Eq)]
pub struct BitswapConfig {
    // pub session: SessionConfig,
}

impl Bitswap {
    /// Create a new `Bitswap`.
    pub fn new(config: BitswapConfig) -> Self {
        Bitswap {
            config,
            ..Default::default()
        }
    }

    pub fn add_peer(&mut self, peer: PeerId) {
        self.known_peers.insert(peer, PeerState::default());
    }

    /// Request the given block from the list of providers.
    #[instrument(skip(self))]
    pub fn want_block<'a>(&mut self, cid: Cid, priority: Priority, providers: HashSet<PeerId>) {
        debug!("want_block: {}", cid);
        for provider in providers.iter() {
            let peer = self.known_peers.entry(*provider).or_default();
            peer.want_block(&cid, priority);
        }

        record!(BitswapMetrics::Providers, providers.len() as u64);
    }

    #[instrument(skip(self, data))]
    pub fn send_block(&mut self, peer_id: &PeerId, cid: Cid, data: Bytes) {
        debug!("send_block: {}", cid);

        record!(BitswapMetrics::BlockBytesOut, data.len() as u64);

        let peer = self.known_peers.entry(*peer_id).or_default();
        peer.send_block(cid, data);
    }

    #[instrument(skip(self))]
    pub fn send_have_block(&mut self, peer_id: &PeerId, cid: Cid) {
        debug!("send_have_block: {}", cid);

        let peer = self.known_peers.entry(*peer_id).or_default();
        peer.send_have_block(cid);
    }

    #[instrument(skip(self))]
    pub fn find_providers(&mut self, cid: Cid, priority: Priority) {
        debug!("find_providers: {}", cid);

        // TODO: better strategies, than just all peers.
        // TODO: use peers that connect later
        let peers: AHashSet<_> = self
            .connected_peers()
            .map(|p| p.to_owned())
            .take(MAX_PROVIDERS)
            .collect();
        debug!("with peers: {:?}", &peers);
        for peer in peers.iter() {
            let peer = self.known_peers.entry(*peer).or_default();
            peer.want_have_block(&cid, priority);
        }
    }

    /// Removes the block from our want list and updates all peers.
    ///
    /// Can be either a user request or be called when the block was received.
    #[instrument(skip(self))]
    pub fn cancel_block(&mut self, cid: &Cid) {
        debug!("cancel_block: {}", cid);
        for state in self.known_peers.values_mut() {
            state.cancel_block(cid);
        }
    }

    #[instrument(skip(self))]
    pub fn cancel_want_block(&mut self, cid: &Cid) {
        debug!("cancel_block: {}", cid);
        for state in self.known_peers.values_mut() {
            state.remove_want_block(cid);
        }
    }

    fn connected_peers(&self) -> impl Iterator<Item = &PeerId> {
        self.known_peers
            .iter()
            .filter_map(|(id, state)| match state.conn {
                ConnState::Connected | ConnState::Unknown => Some(id),
                ConnState::Disconnected | ConnState::Dialing => None,
            })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(clippy::large_enum_variant)]
pub enum HandlerEvent {
    Upgrade,
    Bitswap(BitswapMessage),
}

impl From<Upgrade> for HandlerEvent {
    fn from(_: Upgrade) -> Self {
        HandlerEvent::Upgrade
    }
}

impl From<BitswapMessage> for HandlerEvent {
    fn from(msg: BitswapMessage) -> Self {
        HandlerEvent::Bitswap(msg)
    }
}

impl NetworkBehaviour for Bitswap {
    type ConnectionHandler = BitswapHandler;
    type OutEvent = BitswapEvent;

    fn new_handler(&mut self) -> Self::ConnectionHandler {
        OneShotHandler::new(
            SubstreamProtocol::new(Default::default(), ()),
            OneShotHandlerConfig {
                keep_alive_timeout: Duration::from_secs(30),
                outbound_substream_timeout: Duration::from_secs(30),
                max_dial_negotiated: 64,
            },
        )
    }

    fn addresses_of_peer(&mut self, _peer_id: &PeerId) -> Vec<Multiaddr> {
        Default::default()
    }

    #[instrument(skip(self))]
    fn inject_connection_established(
        &mut self,
        peer_id: &PeerId,
        _conn: &ConnectionId,
        _endpoint: &ConnectedPoint,
        _failed_addresses: Option<&Vec<Multiaddr>>,
        other_established: usize,
    ) {
        let val = self.known_peers.entry(*peer_id).or_default();
        val.conn = ConnState::Connected;
    }

    #[instrument(skip(self, _handler))]
    fn inject_connection_closed(
        &mut self,
        peer_id: &PeerId,
        _conn: &ConnectionId,
        _endpoint: &ConnectedPoint,
        _handler: <Self::ConnectionHandler as IntoConnectionHandler>::Handler,
        remaining_established: usize,
    ) {
        if remaining_established == 0 {
            if let Some(val) = self.known_peers.get_mut(peer_id) {
                val.conn = ConnState::Disconnected;
            }
        }
    }

    #[instrument(skip(self, _handler))]
    fn inject_dial_failure(
        &mut self,
        peer_id: Option<PeerId>,
        _handler: Self::ConnectionHandler,
        error: &DialError,
    ) {
        if let Some(ref peer_id) = peer_id {
            if let DialError::ConnectionLimit(_) = error {
                // we can retry later
                let state = self.known_peers.entry(*peer_id).or_default();
                state.conn = ConnState::Disconnected;
            } else {
                // remove peers we can't dial
                self.known_peers.remove(peer_id);
            }
        }
    }

    #[instrument(skip(self))]
    fn inject_event(&mut self, peer_id: PeerId, connection: ConnectionId, message: HandlerEvent) {
        match message {
            HandlerEvent::Upgrade => {
                // outbound upgrade
            }
            HandlerEvent::Bitswap(mut message) => {
                inc!(BitswapMetrics::Requests);

                // Process incoming message.
                while let Some(block) = message.pop_block() {
                    record!(BitswapMetrics::BlockBytesIn, block.data.len() as u64);

                    for (id, state) in self.known_peers.iter_mut() {
                        if id == &peer_id {
                            state.cancel_block(&block.cid);
                        } else {
                            state.remove_block(&block.cid);
                        }
                    }

                    let event = BitswapEvent::OutboundQueryCompleted {
                        result: QueryResult::Want(WantResult::Ok {
                            sender: peer_id,
                            cid: block.cid,
                            data: block.data.clone(),
                        }),
                    };

                    self.events
                        .push_back(NetworkBehaviourAction::GenerateEvent(event));
                }

                for bp in message.block_presences() {
                    for (_, state) in self.known_peers.iter_mut() {
                        state.remove_want_block(&bp.cid);
                    }

                    let event = BitswapEvent::OutboundQueryCompleted {
                        result: QueryResult::FindProviders(FindProvidersResult::Ok {
                            cid: bp.cid,
                            provider: peer_id,
                        }),
                    };

                    self.events
                        .push_back(NetworkBehaviourAction::GenerateEvent(event));
                }

                // Propagate Want Events
                for (cid, priority) in message.wantlist().blocks() {
                    let event = BitswapEvent::InboundRequest {
                        request: InboundRequest::Want {
                            sender: peer_id,
                            cid: *cid,
                            priority,
                        },
                    };
                    self.events
                        .push_back(NetworkBehaviourAction::GenerateEvent(event));
                }

                // Propagate WantHave Events
                for (cid, priority) in message.wantlist().want_have_blocks() {
                    let event = BitswapEvent::InboundRequest {
                        request: InboundRequest::WantHave {
                            sender: peer_id,
                            cid: *cid,
                            priority,
                        },
                    };
                    self.events
                        .push_back(NetworkBehaviourAction::GenerateEvent(event));
                }

                // TODO: cancel Query::Send

                // Propagate Cancel Events
                for cid in message.wantlist().cancels() {
                    inc!(BitswapMetrics::Cancels);
                    let event = BitswapEvent::InboundRequest {
                        request: InboundRequest::Cancel {
                            sender: peer_id,
                            cid: *cid,
                        },
                    };

                    self.events
                        .push_back(NetworkBehaviourAction::GenerateEvent(event));
                }
            }
        }
    }

    #[allow(clippy::type_complexity)]
    fn poll(
        &mut self,
        _: &mut Context,
        _: &mut impl PollParameters,
    ) -> Poll<NetworkBehaviourAction<Self::OutEvent, Self::ConnectionHandler>> {
        if let Some(event) = self.events.pop_front() {
            return Poll::Ready(event);
        }

        // make progress on connected peers first
        if let Some((peer_id, peer_state)) = self
            .known_peers
            .iter_mut()
            .find(|(_, s)| s.is_connected() && !s.is_empty())
        {
            // connected, send message
            // TODO: limit size
            // TODO: limit how ofen we send

            let msg = peer_state.send_message();
            trace!("sending message to {} {:?}", peer_id, msg);
            return Poll::Ready(NetworkBehaviourAction::NotifyHandler {
                peer_id: *peer_id,
                handler: NotifyHandler::Any,
                event: msg,
            });
        }

        // trigger dials on all peers we need to
        if let Some((peer_id, peer_state)) = self
            .known_peers
            .iter_mut()
            .find(|(_, s)| s.needs_connection())
        {
            // not connected, need to dial
            peer_state.conn = ConnState::Dialing;
            let handler = Default::default();
            return Poll::Ready(NetworkBehaviourAction::Dial {
                opts: DialOpts::peer_id(*peer_id).build(),
                handler,
            });
        }

        Poll::Pending
    }
}

#[cfg(test)]
mod tests {
    use std::io::{Error, ErrorKind};
    use std::sync::atomic::AtomicBool;
    use std::time::Duration;

    use futures::channel::mpsc;
    use futures::prelude::*;
    use libp2p::core::muxing::StreamMuxerBox;
    use libp2p::core::transport::upgrade::Version;
    use libp2p::core::transport::Boxed;
    use libp2p::identity::Keypair;
    use libp2p::swarm::{SwarmBuilder, SwarmEvent};
    use libp2p::tcp::{GenTcpConfig, TokioTcpTransport};
    use libp2p::yamux::YamuxConfig;
    use libp2p::{noise, PeerId, Swarm, Transport};
    use tracing::trace;
    use tracing_subscriber::{fmt, prelude::*, EnvFilter};

    use super::*;
    use crate::block::tests::create_block;
    use crate::Block;

    fn mk_transport() -> (PeerId, Boxed<(PeerId, StreamMuxerBox)>) {
        let local_key = Keypair::generate_ed25519();

        let auth_config = {
            let dh_keys = noise::Keypair::<noise::X25519Spec>::new()
                .into_authentic(&local_key)
                .expect("Noise key generation failed");

            noise::NoiseConfig::xx(dh_keys).into_authenticated()
        };

        let peer_id = local_key.public().to_peer_id();
        let transport = TokioTcpTransport::new(GenTcpConfig::default().nodelay(true))
            .upgrade(Version::V1)
            .authenticate(auth_config)
            .multiplex(YamuxConfig::default())
            .timeout(Duration::from_secs(20))
            .map(|(peer_id, muxer), _| (peer_id, StreamMuxerBox::new(muxer)))
            .map_err(|err| Error::new(ErrorKind::Other, err))
            .boxed();
        (peer_id, transport)
    }

    #[tokio::test]
    async fn test_bitswap_behaviour() {
        tracing_subscriber::registry()
            .with(fmt::layer().pretty())
            .with(EnvFilter::from_default_env())
            .init();

        let (peer1_id, trans) = mk_transport();
        let mut swarm1 = SwarmBuilder::new(trans, Bitswap::default(), peer1_id)
            .executor(Box::new(|fut| {
                tokio::spawn(fut);
            }))
            .build();

        let (peer2_id, trans) = mk_transport();
        let mut swarm2 = SwarmBuilder::new(trans, Bitswap::default(), peer2_id)
            .executor(Box::new(|fut| {
                tokio::spawn(fut);
            }))
            .build();

        let (mut tx, mut rx) = mpsc::channel::<Multiaddr>(1);
        Swarm::listen_on(&mut swarm1, "/ip4/127.0.0.1/tcp/0".parse().unwrap()).unwrap();

        let Block {
            cid: cid_orig,
            data: data_orig,
        } = create_block(&b"hello world"[..]);
        let cid = cid_orig;

        let received_have_orig = AtomicBool::new(false);

        let received_have = &received_have_orig;
        let peer1 = async move {
            while swarm1.next().now_or_never().is_some() {}

            for l in Swarm::listeners(&swarm1) {
                tx.send(l.clone()).await.unwrap();
            }

            loop {
                match swarm1.next().await {
                    Some(SwarmEvent::Behaviour(BitswapEvent::InboundRequest {
                        request:
                            InboundRequest::WantHave {
                                sender,
                                cid,
                                priority,
                            },
                    })) => {
                        trace!("peer1: wanthave: {}", cid);
                        assert_eq!(cid_orig, cid);
                        assert_eq!(priority, 1000);
                        swarm1.behaviour_mut().send_have_block(&sender, cid_orig);
                        received_have.store(true, std::sync::atomic::Ordering::SeqCst);
                    }
                    Some(SwarmEvent::Behaviour(BitswapEvent::InboundRequest {
                        request: InboundRequest::Want { sender, cid, .. },
                    })) => {
                        trace!("peer1: want: {}", cid);
                        assert_eq!(cid_orig, cid);

                        swarm1
                            .behaviour_mut()
                            .send_block(&sender, cid_orig, data_orig.clone());
                    }
                    ev => trace!("peer1: {:?}", ev),
                }
            }
        };

        let peer2 = async move {
            Swarm::dial(&mut swarm2, rx.next().await.unwrap()).unwrap();

            let orig_cid = cid;
            loop {
                match swarm2.next().await {
                    Some(SwarmEvent::ConnectionEstablished {
                        peer_id,
                        num_established,
                        ..
                    }) => {
                        assert_eq!(u32::from(num_established), 1);
                        assert_eq!(peer_id, peer1_id);

                        // wait for the connection to send the want
                        swarm2.behaviour_mut().find_providers(cid, 1000);
                    }
                    Some(SwarmEvent::Behaviour(BitswapEvent::OutboundQueryCompleted {
                        result:
                            QueryResult::FindProviders(FindProvidersResult::Ok { cid, provider }),
                    })) => {
                        trace!("peer2: findproviders: {}", cid);
                        assert_eq!(orig_cid, cid);

                        assert_eq!(provider, peer1_id);

                        assert!(received_have.load(std::sync::atomic::Ordering::SeqCst));

                        swarm2.behaviour_mut().want_block(
                            cid,
                            1000,
                            [peer1_id].into_iter().collect(),
                        );
                    }
                    Some(SwarmEvent::Behaviour(BitswapEvent::OutboundQueryCompleted {
                        result: QueryResult::Want(WantResult::Ok { sender, cid, data }),
                    })) => {
                        assert_eq!(sender, peer1_id);
                        assert_eq!(orig_cid, cid);
                        return data;
                    }
                    ev => trace!("peer2: {:?}", ev),
                }
            }
        };

        let block = future::select(Box::pin(peer1), Box::pin(peer2))
            .await
            .factor_first()
            .0;

        assert!(received_have.load(std::sync::atomic::Ordering::SeqCst));
        assert_eq!(&block[..], b"hello world");
    }
}
