use ahash::{AHashMap, AHashSet};
use bytes::Bytes;
use cid::Cid;
use libp2p::{
    swarm::{NetworkBehaviourAction, NotifyHandler},
    PeerId,
};
use tracing::{error, trace};

use crate::{
    behaviour::{BitswapHandler, FindProvidersResult, QueryError, SendHaveResult},
    message::BlockPresence,
    BitswapEvent, BitswapMessage, Block, CancelResult, Priority, QueryResult, SendResult,
    WantResult,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct QueryId(usize);

#[derive(Default, Debug)]
pub struct QueryManager {
    queries: AHashMap<QueryId, Query>,
    next_id: usize,
}

impl QueryManager {
    fn new_query(&mut self, query: Query) -> QueryId {
        let id = QueryId(self.next_id);
        self.next_id = self.next_id.wrapping_add(1);
        self.queries.insert(id, query);
        id
    }

    pub fn is_empty(&self) -> bool {
        self.queries.is_empty()
    }

    fn cancel_len(&self) -> usize {
        self.queries
            .values()
            .filter(|q| matches!(q, Query::Cancel { .. }))
            .count()
    }

    fn want_len(&self) -> usize {
        self.queries
            .values()
            .filter(|q| matches!(q, Query::Want { .. }))
            .count()
    }

    fn want_have_len(&self) -> usize {
        self.queries
            .values()
            .filter(|q| matches!(q, Query::FindProviders { .. }))
            .count()
    }

    fn send_len(&self) -> usize {
        self.queries
            .values()
            .filter(|q| matches!(q, Query::Send { .. }))
            .count()
    }

    fn send_have_len(&self) -> usize {
        self.queries
            .values()
            .filter(|q| matches!(q, Query::SendHave { .. }))
            .count()
    }

    pub fn want(&mut self, cid: Cid, priority: Priority, providers: AHashSet<PeerId>) -> QueryId {
        self.new_query(Query::Want {
            providers,
            cid,
            priority,
            state: State::New,
        })
    }

    pub fn send(&mut self, receiver: PeerId, cid: Cid, data: Bytes) -> QueryId {
        self.new_query(Query::Send {
            receiver,
            block: Block { cid, data },
            state: State::New,
        })
    }

    pub fn send_have(&mut self, receiver: PeerId, cid: Cid) -> QueryId {
        self.new_query(Query::SendHave {
            receiver,
            cid,
            state: State::New,
        })
    }

    pub fn find_providers(
        &mut self,
        cid: Cid,
        priority: Priority,
        peers: AHashSet<PeerId>,
    ) -> QueryId {
        self.new_query(Query::FindProviders {
            cid,
            peers,
            providers: Default::default(),
            state: State::New,
            priority,
        })
    }

    pub fn cancel(&mut self, cid: &Cid) -> Option<QueryId> {
        let mut cancel = None;
        self.queries.retain(|_, query| match query {
            Query::Want {
                providers: _,
                cid: c,
                priority: _,
                state,
            } => {
                let to_remove = cid == c;
                if to_remove {
                    if let State::Sent(providers) = state {
                        // send out cancels to the providers
                        cancel = Some((providers.clone(), *cid));
                    }
                }
                !to_remove
            }
            Query::FindProviders { .. } => true,
            Query::Cancel { .. } => true,
            Query::Send { .. } => true,
            Query::SendHave { .. } => true,
        });

        cancel.map(|(providers, cid)| {
            self.new_query(Query::Cancel {
                providers,
                cid,
                state: State::New,
            })
        })
    }

    pub fn process_block(&mut self, sender: &PeerId, block: &Block) -> (Vec<PeerId>, Vec<QueryId>) {
        let mut cancels = Vec::new();
        let mut unused_providers = Vec::new();
        let mut query_ids = Vec::new();

        self.queries.retain(|id, query| {
            match query {
                Query::Want {
                    providers,
                    cid,
                    priority: _,
                    state,
                } => {
                    if &block.cid == cid {
                        query_ids.push(*id);
                        for provider in providers.iter() {
                            unused_providers.push(*provider);
                        }

                        if let State::Sent(providers) = state {
                            // send out cancels to the providers
                            let mut providers = providers.clone();
                            providers.remove(sender);
                            if !providers.is_empty() {
                                cancels.push((providers, block.cid));
                            }
                        }
                        false
                    } else {
                        true
                    }
                }
                Query::FindProviders { .. } => true,
                Query::Cancel { .. } => true,
                Query::Send { .. } => true,
                Query::SendHave { .. } => true,
            }
        });

        for (providers, cid) in cancels.into_iter() {
            self.new_query(Query::Cancel {
                providers,
                cid,
                state: State::New,
            });
        }

        (unused_providers, query_ids)
    }

    pub fn process_block_presence(
        &mut self,
        peer: PeerId,
        bp: &BlockPresence,
    ) -> Vec<(QueryId, AHashSet<PeerId>)> {
        let mut results = Vec::new();

        self.queries.retain(|id, query| match query {
            Query::Want { .. } => true,
            Query::FindProviders {
                cid,
                peers,
                providers,
                ..
            } => {
                if bp.is_have() && &bp.cid == cid {
                    providers.insert(peer);

                    if peers.is_empty() || providers.len() >= 40 {
                        results.push((*id, providers.clone()));

                        false
                    } else {
                        true
                    }
                } else {
                    true
                }
            }
            Query::Cancel { .. } => true,
            Query::Send { .. } => true,
            Query::SendHave { .. } => true,
        });

        results
    }

    /// Handle disconnection of the endpoint
    pub fn disconnected(&mut self, peer_id: &PeerId) {
        for (_, query) in self
            .queries
            .iter_mut()
            .filter(|(_, query)| query.contains_provider(peer_id))
        {
            match query {
                Query::Want { state, .. } => {
                    if let State::Sent(used_providers) = state {
                        used_providers.remove(peer_id);
                    }
                }
                Query::FindProviders { state, .. } => {
                    if let State::Sent(used_providers) = state {
                        used_providers.remove(peer_id);
                    }
                }
                Query::Send { state, .. } => {
                    if let State::Sent(used_providers) = state {
                        used_providers.remove(peer_id);
                    }
                }
                Query::SendHave { state, .. } => {
                    if let State::Sent(used_providers) = state {
                        used_providers.remove(peer_id);
                    }
                }
                Query::Cancel { state, .. } => {
                    if let State::Sent(used_providers) = state {
                        used_providers.remove(peer_id);
                    }
                }
            }
        }
    }

    pub fn dial_failure(&mut self, peer_id: &PeerId) {
        self.disconnected(peer_id);
    }

    fn next_finished_query(&mut self) -> Option<(QueryId, Query)> {
        let mut next_query = None;
        for (query_id, query) in &self.queries {
            match query {
                Query::Want {
                    providers, state, ..
                } => {
                    if providers.is_empty() {
                        if let State::Sent(used_providers) = state {
                            if used_providers.is_empty() {
                                next_query = Some(query_id);
                                break;
                            }
                        }
                    }
                }
                Query::FindProviders { state, peers, .. } => {
                    if peers.is_empty() {
                        if let State::Sent(used_providers) = state {
                            if used_providers.is_empty() {
                                next_query = Some(query_id);
                                break;
                            }
                        }
                    }
                }
                Query::Send { state, .. } => {
                    if let State::Sent(providers) = state {
                        if providers.is_empty() {
                            next_query = Some(query_id);
                            break;
                        }
                    }
                }
                Query::SendHave { state, .. } => {
                    if let State::Sent(providers) = state {
                        if providers.is_empty() {
                            next_query = Some(query_id);
                            break;
                        }
                    }
                }
                Query::Cancel {
                    providers, state, ..
                } => {
                    if providers.is_empty() {
                        if let State::Sent(used_providers) = state {
                            if used_providers.is_empty() {
                                next_query = Some(query_id);
                                break;
                            }
                        }
                    }
                }
            }
        }

        if let Some(id) = next_query {
            let id = *id;
            return Some((id, self.queries.remove(&id).unwrap()));
        }

        None
    }

    pub fn poll_all(&mut self) -> Option<NetworkBehaviourAction<BitswapEvent, BitswapHandler>> {
        self.next_finished_query()
            .map(|(id, query)| match query {
                Query::Send { .. } => (id, QueryResult::Send(SendResult::Err(QueryError::Timeout))),
                Query::SendHave { .. } => (
                    id,
                    QueryResult::SendHave(SendHaveResult::Err(QueryError::Timeout)),
                ),
                Query::FindProviders { .. } => (
                    id,
                    QueryResult::FindProviders(FindProvidersResult::Err(QueryError::Timeout)),
                ),
                Query::Want { .. } => (id, QueryResult::Want(WantResult::Err(QueryError::Timeout))),
                Query::Cancel { .. } => (
                    id,
                    QueryResult::Cancel(CancelResult::Err(QueryError::Timeout)),
                ),
            })
            .map(|(_, result)| {
                NetworkBehaviourAction::GenerateEvent(BitswapEvent::OutboundQueryCompleted {
                    result,
                })
            })
    }

    pub fn poll_peer(
        &mut self,
        peer_id: &PeerId,
    ) -> Option<NetworkBehaviourAction<BitswapEvent, BitswapHandler>> {
        if self.is_empty() {
            return None;
        }

        // Aggregate all queries for this peer
        let mut msg = BitswapMessage::default();

        trace!(
            "connected {}, looking for queries: {}want, {}want have, {}cancel, {}send, {}send have",
            peer_id,
            self.want_len(),
            self.want_have_len(),
            self.cancel_len(),
            self.send_len(),
            self.send_have_len()
        );
        let mut num_queries = 0;
        let mut finished_queries = Vec::new();

        for (query_id, query) in self
            .queries
            .iter_mut()
            .filter(|(_, query)| query.contains_unused_provider(peer_id))
        {
            num_queries += 1;
            match query {
                Query::Want {
                    providers,
                    cid,
                    priority,
                    state,
                } => {
                    msg.wantlist_mut().want_block(cid, *priority);

                    providers.remove(peer_id);

                    // update state
                    match state {
                        State::New => {
                            *state = State::Sent([*peer_id].into_iter().collect());
                        }
                        State::Sent(sent_providers) => {
                            sent_providers.insert(*peer_id);
                        }
                    }
                }
                Query::FindProviders {
                    cid,
                    peers,
                    state,
                    priority,
                    ..
                } => {
                    msg.wantlist_mut().want_have_block(cid, *priority);
                    peers.remove(peer_id);

                    // update state
                    match state {
                        State::New => {
                            *state = State::Sent([*peer_id].into_iter().collect());
                        }
                        State::Sent(sent_providers) => {
                            sent_providers.insert(*peer_id);
                        }
                    }
                }
                Query::Cancel {
                    providers,
                    cid,
                    state,
                } => {
                    msg.wantlist_mut().cancel_block(cid);
                    providers.remove(peer_id);

                    // update state
                    match state {
                        State::New => {
                            *state = State::Sent([*peer_id].into_iter().collect());
                        }
                        State::Sent(sent_providers) => {
                            sent_providers.insert(*peer_id);
                        }
                    }
                }
                Query::Send {
                    block,
                    state,
                    receiver,
                } => match state {
                    State::New => {
                        msg.add_block(block.clone());
                        *state = State::Sent([*receiver].into_iter().collect());
                    }
                    State::Sent(_) => {
                        // nothing to do anymore
                        num_queries -= 1;
                        finished_queries.push(*query_id);
                    }
                },
                Query::SendHave {
                    cid,
                    state,
                    receiver,
                } => match state {
                    State::New => {
                        msg.add_block_presence(BlockPresence::have(*cid));
                        *state = State::Sent([*receiver].into_iter().collect());
                    }
                    State::Sent(_) => {
                        // nothing to do anymore
                        num_queries -= 1;
                        finished_queries.push(*query_id);
                    }
                },
            }
        }

        // remove finished queries
        for id in finished_queries {
            self.queries.remove(&id);
        }
        if num_queries > 0 {
            if msg.is_empty() {
                error!("{} queries, but message is empty: {:?}", num_queries, msg);
            } else {
                trace!("sending message to {} {:?}", peer_id, msg);
                return Some(NetworkBehaviourAction::NotifyHandler {
                    peer_id: *peer_id,
                    handler: NotifyHandler::Any,
                    event: msg,
                });
            }
        }

        None
    }
}

#[derive(Debug)]
enum Query {
    /// Fetch a single CID.
    Want {
        providers: AHashSet<PeerId>,
        cid: Cid,
        priority: Priority,
        state: State,
    },
    FindProviders {
        cid: Cid,
        /// peers that are contacted
        peers: AHashSet<PeerId>,
        /// found providers.
        providers: AHashSet<PeerId>,
        state: State,
        priority: Priority,
    },
    /// Cancel a single CID.
    Cancel {
        providers: AHashSet<PeerId>,
        cid: Cid,
        state: State,
    },
    /// Sends a single Block.
    Send {
        receiver: PeerId,
        block: Block,
        state: State,
    },
    /// Sends a want have confirmation.
    SendHave {
        receiver: PeerId,
        cid: Cid,
        state: State,
    },
}

impl Query {
    fn contains_unused_provider(&self, peer_id: &PeerId) -> bool {
        match self {
            Query::Want { providers, .. } | Query::Cancel { providers, .. } => {
                providers.contains(peer_id)
            }
            Query::FindProviders { peers, .. } => peers.contains(peer_id),
            Query::Send { receiver, .. } => receiver == peer_id,
            Query::SendHave { receiver, .. } => receiver == peer_id,
        }
    }

    fn contains_provider(&self, peer_id: &PeerId) -> bool {
        match self {
            Query::Want {
                providers, state, ..
            }
            | Query::Cancel {
                providers, state, ..
            } => {
                if providers.contains(peer_id) {
                    return true;
                }
                if let State::Sent(p) = state {
                    return p.contains(peer_id);
                }
                false
            }
            Query::FindProviders { peers, state, .. } => {
                if peers.contains(peer_id) {
                    return true;
                }
                if let State::Sent(p) = state {
                    return p.contains(peer_id);
                }
                false
            }
            Query::Send {
                receiver, state, ..
            } => {
                if receiver == peer_id {
                    return true;
                }
                if let State::Sent(p) = state {
                    return p.contains(peer_id);
                }
                false
            }
            Query::SendHave {
                receiver, state, ..
            } => {
                if receiver == peer_id {
                    return true;
                }
                if let State::Sent(p) = state {
                    return p.contains(peer_id);
                }
                false
            }
        }
    }
}

#[derive(Debug)]
enum State {
    New,
    Sent(AHashSet<PeerId>),
}

#[cfg(test)]
mod tests {
    use libp2p::identity::Keypair;

    use super::*;
    use crate::block::tests::create_block;

    #[test]
    fn test_want_success() {
        let mut queries = QueryManager::default();

        let provider_key_1 = Keypair::generate_ed25519();
        let provider_id_1 = provider_key_1.public().to_peer_id();
        let provider_key_2 = Keypair::generate_ed25519();
        let provider_id_2 = provider_key_2.public().to_peer_id();

        assert!(queries.poll_peer(&provider_id_1).is_none());

        let Block { cid, data } = create_block(&b"hello world"[..]);
        let query_id = queries.want(
            cid,
            100,
            [provider_id_1, provider_id_2].into_iter().collect(),
        );

        // sent wantlist
        let q = queries.poll_peer(&provider_id_1).unwrap();
        if let NetworkBehaviourAction::NotifyHandler { peer_id, event, .. } = q {
            assert_eq!(peer_id, provider_id_1);
            assert_eq!(
                event.wantlist().blocks().collect::<Vec<_>>(),
                &[(&cid, 100)]
            );
        } else {
            panic!("invalid poll result");
        }

        // inject received block
        let (unused_providers, qid) = queries.process_block(&provider_id_1, &Block { cid, data });
        assert_eq!(unused_providers, &[provider_id_2]);
        assert_eq!(qid, vec![query_id]);
    }

    #[test]
    fn test_want_fail() {
        let mut queries = QueryManager::default();

        let provider_key_1 = Keypair::generate_ed25519();
        let provider_id_1 = provider_key_1.public().to_peer_id();
        let provider_key_2 = Keypair::generate_ed25519();
        let provider_id_2 = provider_key_2.public().to_peer_id();

        assert!(queries.poll_peer(&provider_id_1).is_none());

        let Block { cid, data: _ } = create_block(&b"hello world"[..]);
        let query_id = queries.want(
            cid,
            100,
            [provider_id_1, provider_id_2].into_iter().collect(),
        );

        // send wantlist
        let q = queries.poll_peer(&provider_id_1).unwrap();
        if let NetworkBehaviourAction::NotifyHandler { peer_id, event, .. } = q {
            assert_eq!(peer_id, provider_id_1);
            assert_eq!(
                event.wantlist().blocks().collect::<Vec<_>>(),
                &[(&cid, 100)]
            );
        } else {
            panic!("invalid poll result");
        }

        let q = queries.poll_peer(&provider_id_2).unwrap();
        if let NetworkBehaviourAction::NotifyHandler { peer_id, event, .. } = q {
            assert_eq!(peer_id, provider_id_2);
            assert_eq!(
                event.wantlist().blocks().collect::<Vec<_>>(),
                &[(&cid, 100)]
            );
        } else {
            panic!("invalid poll result");
        }

        // inject disconnects
        queries.disconnected(&provider_id_1);
        queries.disconnected(&provider_id_2);

        let q = queries.poll_all().unwrap();
        if let NetworkBehaviourAction::GenerateEvent(BitswapEvent::OutboundQueryCompleted {
            ..
        }) = q
        {
            //
        } else {
            panic!("invalid poll result");
        }
    }
}
