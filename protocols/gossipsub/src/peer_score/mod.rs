//! Manages and stores the Scoring logic of a particular peer on the gossipsub behaviour.

use crate::{GossipsubMessage, Hasher, MessageId, Topic, TopicHash};
use libp2p_core::PeerId;
use log::warn;
use lru_time_cache::LruCache;
use std::collections::{HashMap, HashSet};
use std::net::IpAddr;
use std::time::{Duration, Instant};

mod params;
use params::*;

#[cfg(test)]
mod tests;

/// The number of seconds delivery messages are stored in the cache.
const TIME_CACHE_DURATION: u64 = 120;

struct PeerScore {
    params: PeerScoreParams,
    /// The score parameters.
    peer_stats: HashMap<PeerId, PeerStats>,
    /// Tracking peers per IP.
    peer_ips: HashMap<IpAddr, HashSet<PeerId>>,
    /// Message delivery tracking. This is a time-cache of `DeliveryRecord`s.
    deliveries: LruCache<MessageId, DeliveryRecord>,
    /// The message id function.
    msg_id: fn(&GossipsubMessage) -> MessageId,
}

/// General statistics for a given gossipsub peer.
struct PeerStats {
    /// Connection status of the peer.
    status: ConnectionStatus,
    /// Stats per topic.
    topics: HashMap<TopicHash, TopicStats>,
    /// IP tracking for individual peers.
    known_ips: Vec<IpAddr>,
    /// Behaviour penalty that is applied to the peer, assigned by the behaviour.
    behaviour_penalty: f64,
}

enum ConnectionStatus {
    /// The peer is connected.
    Connected,
    /// The peer is disconnected
    Disconnected {
        /// Expiration time of the score state for disconnected peers.
        expire: Instant,
    },
}

impl Default for PeerStats {
    fn default() -> Self {
        PeerStats {
            status: ConnectionStatus::Connected,
            topics: HashMap::new(),
            known_ips: Vec::new(),
            behaviour_penalty: 0f64,
        }
    }
}

impl PeerStats {
    /// Returns a mutable reference to topic stats if they exist, otherwise if the supplied parameters score the
    /// topic, inserts the default stats and returns a reference to those. If neither apply, returns None.
    pub fn stats_or_default_mut(
        &mut self,
        topic_hash: TopicHash,
        params: &PeerScoreParams,
    ) -> Option<&mut TopicStats> {
        if params.topics.get(&topic_hash).is_some() {
            Some(self.topics.entry(topic_hash).or_default())
        } else {
            self.topics.get_mut(&topic_hash)
        }
    }
}

/// Stats assigned to peer for each topic.
struct TopicStats {
    mesh_status: MeshStatus,
    /// Number of first message deliveries.
    first_message_deliveries: f64,
    /// True if the peer has been in the mesh for enough time to activate mesh message deliveries.
    mesh_message_deliveries_active: bool,
    /// Number of message deliveries from the mesh.
    mesh_message_deliveries: f64,
    /// Mesh rate failure penalty.
    mesh_failure_penalty: f64,
    /// Invalid message counter.
    invalid_message_deliveries: f64,
}

impl TopicStats {
    /// Returns true if the peer is in the `mesh`.
    pub fn in_mesh(&self) -> bool {
        if let MeshStatus::Active { .. } = self.mesh_status {
            true
        } else {
            false
        }
    }
}

/// Status defining a peer's inclusion in the mesh and associated parameters.
enum MeshStatus {
    Active {
        /// The time the peer was last GRAFTed;
        graft_time: Instant,
        /// The time the peer has been in the mesh.
        mesh_time: Duration,
    },
    InActive,
}

impl MeshStatus {
    /// Initialises a new `Active` mesh status.
    pub fn new_active() -> Self {
        MeshStatus::Active {
            graft_time: Instant::now(),
            mesh_time: Duration::from_secs(0),
        }
    }
}

impl Default for TopicStats {
    fn default() -> Self {
        TopicStats {
            mesh_status: MeshStatus::InActive,
            first_message_deliveries: Default::default(),
            mesh_message_deliveries_active: Default::default(),
            mesh_message_deliveries: Default::default(),
            mesh_failure_penalty: Default::default(),
            invalid_message_deliveries: Default::default(),
        }
    }
}

#[derive(PartialEq, Debug)]
struct DeliveryRecord {
    status: DeliveryStatus,
    first_seen: Instant,
    validated: Instant,
    peers: HashSet<PeerId>,
}

#[derive(PartialEq, Debug)]
enum DeliveryStatus {
    /// Don't know (yet) if the message is valid.
    Unknown,
    /// The message is valid.
    Valid,
    /// The message is invalid.
    Invalid,
    /// Instructed by the validator to ignore the message.
    Ignored,
    /// Can't tell if the message is valid because validation was throttled.
    Throttled,
}

impl Default for DeliveryRecord {
    fn default() -> Self {
        DeliveryRecord {
            status: DeliveryStatus::Unknown,
            first_seen: Instant::now(),
            validated: Instant::now(),
            peers: HashSet::new(),
        }
    }
}

impl PeerScore {
    /// Creates a new `PeerScore` using a given set of peer scoring parameters.
    pub fn new(params: PeerScoreParams) -> Self {
        let default_message_id = |message: &GossipsubMessage| {
            // default message id is: source + sequence number
            let mut source_string = message.source.to_base58();
            source_string.push_str(&message.sequence_number.to_string());
            MessageId(source_string)
        };

        PeerScore {
            params,
            peer_stats: HashMap::new(),
            peer_ips: HashMap::new(),
            deliveries: LruCache::with_expiry_duration(Duration::from_secs(TIME_CACHE_DURATION)),
            msg_id: default_message_id,
        }
    }

    /// Creates a new `PeerScore` with a non-default message id function.
    pub fn new_with_msg_id(
        params: PeerScoreParams,
        msg_id: fn(&GossipsubMessage) -> MessageId,
    ) -> Self {
        PeerScore {
            params,
            peer_stats: HashMap::new(),
            peer_ips: HashMap::new(),
            deliveries: LruCache::with_expiry_duration(Duration::from_secs(TIME_CACHE_DURATION)),
            msg_id,
        }
    }

    /// Returns the score for a peer.
    pub fn score(&self, peer_id: &PeerId) -> f64 {
        let peer_stats = match self.peer_stats.get(peer_id) {
            Some(v) => v,
            None => return 0.0,
        };

        let mut score = 0.0;

        // topic scores
        for (topic, topic_stats) in peer_stats.topics.iter() {
            // topic parameters
            if let Some(topic_params) = self.params.topics.get(topic) {
                // we are tracking the topic

                // the topic score
                let mut topic_score = 0.0;

                // P1: time in mesh
                if let MeshStatus::Active { mesh_time, .. } = topic_stats.mesh_status {
                    let p1 = {
                        let v = mesh_time.as_secs_f64()
                            / topic_params.time_in_mesh_quantum.as_secs_f64();
                        if v < topic_params.time_in_mesh_cap {
                            v
                        } else {
                            topic_params.time_in_mesh_cap
                        }
                    };
                    dbg!(topic_score);
                    topic_score += p1 * topic_params.time_in_mesh_weight;
                    dbg!(topic_score);
                }

                // P2: first message deliveries
                let p2 = topic_stats.first_message_deliveries as f64;
                topic_score += p2 * topic_params.first_message_deliveries_weight;
                dbg!(topic_score);

                // P3: mesh message deliveries
                if topic_stats.mesh_message_deliveries_active {
                    if topic_stats.mesh_message_deliveries
                        < topic_params.mesh_message_deliveries_threshold
                    {
                        let deficit = topic_params.mesh_message_deliveries_threshold
                            - topic_stats.mesh_message_deliveries;
                        let p3 = deficit * deficit;
                        topic_score += p3 * topic_params.mesh_message_deliveries_weight;
                    }
                }
                dbg!(topic_score);

                // P3b:
                // NOTE: the weight of P3b is negative (validated in TopicScoreParams.validate), so this detracts.
                let p3b = topic_stats.mesh_failure_penalty;
                topic_score += p3b * topic_params.mesh_failure_penalty_weight;

                // P4: invalid messages
                // NOTE: the weight of P4 is negative (validated in TopicScoreParams.validate), so this detracts.
                let p4 =
                    topic_stats.invalid_message_deliveries * topic_stats.invalid_message_deliveries;
                topic_score += p4 * topic_params.invalid_message_deliveries_weight;
                dbg!(topic_score);

                // update score, mixing with topic weight
                score += topic_score * topic_params.topic_weight;
                dbg!(topic_score);
            }
        }

        // apply the topic score cap, if any
        if self.params.topic_score_cap > 0f64 && score > self.params.topic_score_cap {
            score = self.params.topic_score_cap;
        }
        dbg!("after");
        dbg!(score);

        // P5: application-specific score
        //TODO: Add in
        /*
        let p5 = self.params.app_specific_score(peer_id);
        score += p5 * self.params.app_specific_weight;
            */

        // P6: IP collocation factor
        for ip in peer_stats.known_ips.iter() {
            if self.params.ip_colocation_factor_whitelist.get(ip).is_some() {
                continue;
            }

            // P6 has a cliff (ip_colocation_factor_threshold); it's only applied iff
            // at least that many peers are connected to us from that source IP
            // addr. It is quadratic, and the weight is negative (validated by
            // peer_score_params.validate()).
            if let Some(peers_in_ip) = self.peer_ips.get(ip).map(|peers| peers.len()) {
                if (peers_in_ip as f64) > self.params.ip_colocation_factor_threshold {
                    let surplus = (peers_in_ip as f64) - self.params.ip_colocation_factor_threshold;
                    let p6 = surplus * surplus;
                    score += p6 * self.params.ip_colocation_factor_weight;
                }
            }
        }

        // P7: behavioural pattern penalty
        let p7 = peer_stats.behaviour_penalty * peer_stats.behaviour_penalty;
        score += p7 * self.params.behaviour_penalty_weight;
        score
    }

    pub fn add_penalty(&mut self, peer_id: &PeerId, count: usize) {
        if let Some(peer_stats) = self.peer_stats.get_mut(peer_id) {
            peer_stats.behaviour_penalty += count as f64;
        }
    }

    pub fn refresh_scores(&mut self) {
        let now = Instant::now();
        let params_ref = &self.params;
        let peer_ips_ref = &mut self.peer_ips;
        self.peer_stats.retain(|peer_id, peer_stats| {
            if let ConnectionStatus::Disconnected { expire } = peer_stats.status {
                // has the retention period expired?
                if now > expire {
                    // yes, throw it away (but clean up the IP tracking first)
                    for ip in peer_stats.known_ips.iter() {
                        if let Some(peer_set) = peer_ips_ref.get_mut(ip) {
                            peer_set.remove(peer_id);
                        }
                    }
                    // re address this, use retain or entry
                    return false;
                }

                // we don't decay retained scores, as the peer is not active.
                // this way the peer cannot reset a negative score by simply disconnecting and reconnecting,
                // unless the retention period has elapsed.
                // similarly, a well behaved peer does not lose its score by getting disconnected.
                return true;
            }

            for (topic, topic_stats) in peer_stats.topics.iter_mut() {
                // the topic parameters
                if let Some(topic_params) = params_ref.topics.get(topic) {
                    // decay counters
                    topic_stats.first_message_deliveries *=
                        topic_params.first_message_deliveries_decay;
                    if topic_stats.first_message_deliveries < params_ref.decay_to_zero {
                        topic_stats.first_message_deliveries = 0.0;
                    }
                    topic_stats.mesh_message_deliveries *=
                        topic_params.mesh_message_deliveries_decay;
                    if topic_stats.mesh_message_deliveries < params_ref.decay_to_zero {
                        topic_stats.mesh_message_deliveries = 0.0;
                    }
                    topic_stats.mesh_failure_penalty *= topic_params.mesh_failure_penalty_decay;
                    if topic_stats.mesh_failure_penalty < params_ref.decay_to_zero {
                        topic_stats.mesh_failure_penalty = 0.0;
                    }
                    topic_stats.invalid_message_deliveries *=
                        topic_params.invalid_message_deliveries_decay;
                    if topic_stats.invalid_message_deliveries < params_ref.decay_to_zero {
                        topic_stats.invalid_message_deliveries = 0.0;
                    }
                    // update mesh time and activate mesh message delivery parameter if need be
                    if let MeshStatus::Active {
                        ref mut mesh_time,
                        ref mut graft_time,
                    } = topic_stats.mesh_status
                    {
                        *mesh_time = now.duration_since(*graft_time);
                        if *mesh_time > topic_params.mesh_message_deliveries_activation {
                            topic_stats.mesh_message_deliveries_active = true;
                        }
                    }
                }
            }

            // decay P7 counter
            peer_stats.behaviour_penalty *= params_ref.behaviour_penalty_decay;
            if peer_stats.behaviour_penalty < params_ref.decay_to_zero {
                peer_stats.behaviour_penalty = 0.0;
            }
            return true;
        });
    }

    /// Gets a mutable reference to the underlying IPs for a peer, if they exist.
    pub fn get_ips_mut(&mut self, peer_id: &PeerId) -> Option<&mut [IpAddr]> {
        let peer_stats = self.peer_stats.get_mut(peer_id)?;
        Some(&mut peer_stats.known_ips)
    }

    /// Adds a connected peer to `PeerScore`, initialising with default stats.
    pub fn add_peer(&mut self, peer_id: PeerId, known_ips: Vec<IpAddr>) {
        let peer_stats = self.peer_stats.entry(peer_id.clone()).or_default();

        // mark the peer as connected
        peer_stats.status = ConnectionStatus::Connected;
        peer_stats.known_ips = known_ips.clone();

        // add known ips to the peer score tracking map
        for ip in known_ips {
            self.peer_ips
                .entry(ip)
                .or_insert_with(|| HashSet::new())
                .insert(peer_id.clone());
        }
    }

    /// Removes a peer from the score table. This retains peer statistics if their score is
    /// non-positive.
    pub fn remove_peer(&mut self, peer_id: &PeerId) {
        // we only retain non-positive scores of peers
        if self.score(peer_id) > 0f64 {
            self.peer_stats.remove(peer_id);
            return;
        }

        // if the peer is retained (including it's score) the `first_message_delivery` counters
        // are reset to 0 and mesh delivery penalties applied.
        if let Some(peer_stats) = self.peer_stats.get_mut(peer_id) {
            for (topic, topic_stats) in peer_stats.topics.iter_mut() {
                topic_stats.first_message_deliveries = 0f64;

                if let Some(threshold) = self
                    .params
                    .topics
                    .get(topic)
                    .map(|param| param.mesh_message_deliveries_threshold)
                {
                    if topic_stats.in_mesh()
                        && topic_stats.mesh_message_deliveries_active
                        && topic_stats.mesh_message_deliveries < threshold
                    {
                        let deficit = threshold - topic_stats.mesh_message_deliveries;
                        topic_stats.mesh_failure_penalty += deficit * deficit;
                    }
                }

                topic_stats.mesh_status = MeshStatus::InActive;
            }

            peer_stats.status = ConnectionStatus::Disconnected {
                expire: Instant::now() + self.params.retain_score,
            };
        }
    }

    /// Handles peer scoring functionality on subscription.
    pub fn join<H: Hasher>(&mut self, _topic: Topic<H>) {}

    /// Handles peer scoring functionality when un-subscribing from a topic.
    pub fn leave<H: Hasher>(&mut self, _topic: Topic<H>) {}

    /// Handles scoring functionality as a peer GRAFTs to a topic.
    pub fn graft(&mut self, peer_id: &PeerId, topic: impl Into<TopicHash>) {
        let topic = topic.into();
        if let Some(peer_stats) = self.peer_stats.get_mut(peer_id) {
            // if we are scoring the topic, update the mesh status.
            if let Some(topic_stats) = peer_stats.stats_or_default_mut(topic, &self.params) {
                topic_stats.mesh_status = MeshStatus::new_active();
                topic_stats.mesh_message_deliveries_active = false;
            }
        }
    }

    /// Handles scoring functionality as a peer PRUNEs from a topic.
    pub fn prune(&mut self, peer_id: &PeerId, topic: TopicHash) {
        if let Some(peer_stats) = self.peer_stats.get_mut(peer_id) {
            // if we are scoring the topic, update the mesh status.
            if let Some(topic_stats) = peer_stats.stats_or_default_mut(topic.clone(), &self.params)
            {
                // sticky mesh delivery rate failure penalty
                let threshold = self
                    .params
                    .topics
                    .get(&topic)
                    .expect("Topic must exist in order for there to be topic stats")
                    .mesh_message_deliveries_threshold;
                if topic_stats.mesh_message_deliveries_active
                    && topic_stats.mesh_message_deliveries < threshold
                {
                    let deficit = threshold - topic_stats.mesh_message_deliveries;
                    topic_stats.mesh_failure_penalty += deficit * deficit;
                }
                topic_stats.mesh_message_deliveries_active = false;
            }
        }
    }

    //TODO: Required?
    pub fn validate_message(&mut self, _from: &PeerId, _msg: &GossipsubMessage) {
        // adds an empty record with the message id
    }

    pub fn deliver_message(&mut self, from: &PeerId, msg: &GossipsubMessage) {
        self.mark_first_message_delivery(from, msg);

        let record = self
            .deliveries
            .entry((self.msg_id)(msg))
            .or_insert_with(|| DeliveryRecord::default());

        // this should be the first delivery trace
        if record.status != DeliveryStatus::Unknown {
            warn!("Unexpected delivery trace: Message from {} was first seen {}s ago and has a delivery status {:?}", from, record.first_seen.elapsed().as_secs(), record.status);
            return;
        }

        // mark the message as valid and reward mesh peers that have already forwarded it to us
        record.status = DeliveryStatus::Valid;
        record.validated = Instant::now();
        for peer in record.peers.iter().cloned().collect::<Vec<_>>() {
            // this check is to make sure a peer can't send us a message twice and get a double
            // count if it is a first delivery
            if &peer != from {
                self.mark_duplicate_message_delivery(&peer, msg, None);
            }
        }
    }

    pub fn reject_message(&mut self, from: &PeerId, msg: &GossipsubMessage, reason: RejectMsg) {
        match reason {
            // these messages are not tracked, but the peer is penalized as they are invalid
            RejectMsg::MissingSignature => {}
            RejectMsg::InvalidSignature => {}
            RejectMsg::SelfOrigin => {
                self.mark_invalid_message_delivery(from, msg);
                return;
            }
            RejectMsg::BlacklistedPeer => {}
            RejectMsg::BlackListenSource => {
                return;
            }
            RejectMsg::ValidationQueueFull => {
                // the message was rejected before it entered the validation pipeline;
                // we don't know if this message has a valid signature, and thus we also don't know if
                // it has a valid message ID; all we can do is ignore it.
                return;
            }
            _ => {} // the rest are handled after record creation
        }

        let mut record = self
            .deliveries
            .remove(&(self.msg_id)(msg))
            .unwrap_or_else(|| DeliveryRecord::default());
        // this should be the first delivery trace
        if record.status != DeliveryStatus::Unknown {
            warn!("Unexpected delivery trace: Message from {} was first seen {}s ago and has a delivery status {:?}", from, record.first_seen.elapsed().as_secs(), record.status);
            self.deliveries.insert((self.msg_id)(msg), record);
            return;
        }

        match reason {
            RejectMsg::ValidationThrottled => {
                // if we reject with "validation throttled" we don't penalize the peer(s) that forward it
                // because we don't know if it was valid.
                record.status = DeliveryStatus::Throttled;
                // release the delivery time tracking map to free some memory early
                record.peers.clear();
                self.deliveries.insert((self.msg_id)(msg), record);
                return;
            }
            RejectMsg::ValidationIgnored => {
                // we were explicitly instructed by the validator to ignore the message but not penalize
                // the peer
                record.status = DeliveryStatus::Ignored;
                record.peers.clear();
                self.deliveries.insert((self.msg_id)(msg), record);
                return;
            }
            _ => {}
        }

        // mark the message as invalid and penalize peers that have already forwarded it.
        record.status = DeliveryStatus::Invalid;

        self.mark_invalid_message_delivery(from, msg);
        for peer_id in record.peers.iter() {
            self.mark_invalid_message_delivery(peer_id, msg)
        }

        // release the delivery time tracking map to free some memory early
        record.peers.clear();
        self.deliveries.insert((self.msg_id)(msg), record);
    }

    pub fn duplicated_message(&mut self, from: &PeerId, msg: &GossipsubMessage) {
        let record = self
            .deliveries
            .entry((self.msg_id)(msg))
            .or_insert_with(|| DeliveryRecord::default());

        if record.peers.get(from).is_some() {
            // we have already seen this duplicate!
            return;
        }

        match record.status {
            DeliveryStatus::Unknown => {
                // the message is being validated; track the peer delivery and wait for
                // the Deliver/Reject notification.
                record.peers.remove(from);
            }
            DeliveryStatus::Valid => {
                // mark the peer delivery time to only count a duplicate delivery once.
                record.peers.remove(from);
                let validated = record.validated.clone();
                self.mark_duplicate_message_delivery(from, msg, Some(validated));
            }
            DeliveryStatus::Invalid => {
                // we no longer track delivery time
                self.mark_invalid_message_delivery(from, msg);
            }
            DeliveryStatus::Throttled | DeliveryStatus::Ignored => {
                // the message was throttled or ignored; do nothing (we don't know if it was valid)
            }
        }
    }

    /// Increments the "invalid message deliveries" counter for all scored topics the message
    /// is published in.
    fn mark_invalid_message_delivery(&mut self, peer_id: &PeerId, msg: &GossipsubMessage) {
        if let Some(peer_stats) = self.peer_stats.get_mut(peer_id) {
            for topic_hash in msg.topics.iter() {
                if let Some(topic_stats) =
                    peer_stats.stats_or_default_mut(topic_hash.clone(), &self.params)
                {
                    topic_stats.invalid_message_deliveries += 1f64;
                }
            }
        }
    }

    /// Increments the "first message deliveries" counter for all scored topics the message is
    /// published in, as well as the "mesh message deliveries" counter, if the peer is in the
    /// mesh for the topic.
    fn mark_first_message_delivery(&mut self, peer_id: &PeerId, msg: &GossipsubMessage) {
        if let Some(peer_stats) = self.peer_stats.get_mut(peer_id) {
            for topic_hash in msg.topics.iter() {
                if let Some(topic_stats) =
                    peer_stats.stats_or_default_mut(topic_hash.clone(), &self.params)
                {
                    let cap = self
                        .params
                        .topics
                        .get(topic_hash)
                        .expect("Topic must exist if there are known topic_stats")
                        .first_message_deliveries_cap;
                    topic_stats.first_message_deliveries =
                        if topic_stats.first_message_deliveries + 1f64 > cap {
                            cap
                        } else {
                            topic_stats.first_message_deliveries + 1f64
                        };

                    if let MeshStatus::Active { .. } = topic_stats.mesh_status {
                        let cap = self
                            .params
                            .topics
                            .get(topic_hash)
                            .expect("Topic must exist if there are known topic_stats")
                            .mesh_message_deliveries_cap;

                        topic_stats.mesh_message_deliveries =
                            if topic_stats.mesh_message_deliveries + 1f64 > cap {
                                cap
                            } else {
                                topic_stats.first_message_deliveries + 1f64
                            };
                    }
                }
            }
        }
    }

    /// Increments the "mesh message deliveries" counter for messages we've seen before, as long the
    /// message was received within the P3 window.
    fn mark_duplicate_message_delivery(
        &mut self,
        peer_id: &PeerId,
        msg: &GossipsubMessage,
        validated_time: Option<Instant>,
    ) {
        if let Some(peer_stats) = self.peer_stats.get_mut(peer_id) {
            for topic_hash in msg.topics.iter() {
                if let Some(topic_stats) =
                    peer_stats.stats_or_default_mut(topic_hash.clone(), &self.params)
                {
                    if let MeshStatus::Active { .. } = topic_stats.mesh_status {
                        let topic_params = self
                            .params
                            .topics
                            .get(topic_hash)
                            .expect("Topic must exist if there are known topic_stats");

                        // check against the mesh delivery window -- if the validated time is passed as 0, then
                        // the message was received before we finished validation and thus falls within the mesh
                        // delivery window.
                        if let Some(validated_time) = validated_time {
                            let now = Instant::now();
                            let window_time = validated_time
                                .checked_add(topic_params.mesh_message_deliveries_window)
                                .unwrap_or_else(|| now.clone());
                            if now > window_time {
                                continue;
                            }

                            let cap = topic_params.mesh_message_deliveries_cap;
                            topic_stats.mesh_message_deliveries =
                                if { topic_stats.mesh_message_deliveries + 1f64 > cap } {
                                    cap
                                } else {
                                    topic_stats.mesh_message_deliveries + 1f64
                                };
                        }
                    }
                }
            }
        }
    }

    /// Removes an IP list from the tracking list for a peer.
    fn remove_ips(&mut self, peer_id: &PeerId, ips: Vec<IpAddr>) {
        for ip in ips {
            if let Some(peer_set) = self.peer_ips.get_mut(&ip) {
                peer_set.remove(peer_id);
            }
        }
    }
}

enum RejectMsg {
    MissingSignature,
    InvalidSignature,
    SelfOrigin,
    BlacklistedPeer,
    BlackListenSource,
    ValidationQueueFull,
    ValidationThrottled,
    ValidationIgnored,
}
