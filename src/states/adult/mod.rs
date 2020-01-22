// Copyright 2019 MaidSafe.net limited.
//
// This SAFE Network Software is licensed to you under The General Public License (GPL), version 3.
// Unless required by applicable law or agreed to in writing, the SAFE Network Software distributed
// under the GPL Licence is distributed on an "AS IS" BASIS, WITHOUT WARRANTIES OR CONDITIONS OF ANY
// KIND, either express or implied. Please review the Licences for the specific language governing
// permissions and limitations relating to use of the SAFE Network Software.

#[cfg(all(test, feature = "mock_base"))]
mod tests;

use super::{
    bootstrapping_peer::{BootstrappingPeer, BootstrappingPeerDetails},
    common::{Approved, Base},
    elder::{Elder, ElderDetails},
};
use crate::{
    chain::{
        Chain, EldersChange, EldersInfo, GenesisPfxInfo, NetworkParams, OnlinePayload,
        SectionKeyInfo, SendAckMessagePayload,
    },
    error::RoutingError,
    event::Event,
    id::{FullId, P2pNode, PublicId},
    location::Location,
    messages::{
        BootstrapResponse, DirectMessage, HopMessageWithBytes, MessageContent,
        SignedRoutingMessage, VerifyStatus,
    },
    network_service::NetworkService,
    outbox::EventBox,
    parsec::{DkgResultWrapper, ParsecMap},
    pause::PausedState,
    peer_map::PeerMap,
    relocation::{RelocateDetails, SignedRelocateDetails},
    rng::{self, MainRng},
    routing_message_filter::RoutingMessageFilter,
    signature_accumulator::SignatureAccumulator,
    state_machine::{State, Transition},
    time::Duration,
    timer::Timer,
    utils::LogIdent,
    xor_space::{Prefix, XorName},
    ConnectionInfo,
};
use itertools::Itertools;
use std::{
    collections::{BTreeSet, VecDeque},
    fmt::{self, Display, Formatter},
    mem,
    net::SocketAddr,
};

// Send our knowledge in a similar speed as GOSSIP_TIMEOUT
const KNOWLEDGE_TIMEOUT: Duration = Duration::from_secs(2);

pub struct AdultDetails {
    pub network_service: NetworkService,
    pub event_backlog: Vec<Event>,
    pub full_id: FullId,
    pub gen_pfx_info: GenesisPfxInfo,
    pub routing_msg_backlog: Vec<SignedRoutingMessage>,
    pub direct_msg_backlog: Vec<(P2pNode, DirectMessage)>,
    pub sig_accumulator: SignatureAccumulator,
    pub routing_msg_filter: RoutingMessageFilter,
    pub timer: Timer,
    pub network_cfg: NetworkParams,
    pub rng: MainRng,
}

pub struct Adult {
    chain: Chain,
    network_service: NetworkService,
    event_backlog: Vec<Event>,
    full_id: FullId,
    gen_pfx_info: GenesisPfxInfo,
    /// Routing messages addressed to us that we cannot handle until we are established.
    routing_msg_backlog: Vec<SignedRoutingMessage>,
    direct_msg_backlog: Vec<(P2pNode, DirectMessage)>,
    sig_accumulator: SignatureAccumulator,
    parsec_map: ParsecMap,
    knowledge_timer_token: u64,
    routing_msg_filter: RoutingMessageFilter,
    timer: Timer,
    rng: MainRng,
}

impl Adult {
    pub fn new(
        mut details: AdultDetails,
        parsec_map: ParsecMap,
        _outbox: &mut dyn EventBox,
    ) -> Result<Self, RoutingError> {
        let public_id = *details.full_id.public_id();
        let knowledge_timer_token = details.timer.schedule(KNOWLEDGE_TIMEOUT);

        let parsec_map = parsec_map.with_init(
            &mut details.rng,
            details.full_id.clone(),
            &details.gen_pfx_info,
        );

        let chain = Chain::new(
            details.network_cfg,
            public_id,
            details.gen_pfx_info.clone(),
            None,
        );

        let node = Self {
            chain,
            network_service: details.network_service,
            event_backlog: details.event_backlog,
            full_id: details.full_id,
            gen_pfx_info: details.gen_pfx_info,
            routing_msg_backlog: details.routing_msg_backlog,
            direct_msg_backlog: details.direct_msg_backlog,
            sig_accumulator: details.sig_accumulator,
            parsec_map,
            routing_msg_filter: details.routing_msg_filter,
            timer: details.timer,
            knowledge_timer_token,
            rng: details.rng,
        };

        Ok(node)
    }

    pub fn closest_known_elders_to(&self, _name: &XorName) -> impl Iterator<Item = &P2pNode> {
        self.chain.our_elders()
    }

    pub fn rebootstrap(mut self) -> Result<State, RoutingError> {
        let network_cfg = self.chain.network_cfg();

        // Try to join the same section, but using new id, otherwise the section won't accept us
        // due to duplicate votes.
        let range_inclusive = self.our_prefix().range_inclusive();
        let full_id = FullId::within_range(&mut self.rng, &range_inclusive);

        Ok(State::BootstrappingPeer(BootstrappingPeer::new(
            BootstrappingPeerDetails {
                network_service: self.network_service,
                full_id,
                network_cfg,
                timer: self.timer,
                rng: self.rng,
            },
        )))
    }

    pub fn relocate(
        self,
        conn_infos: Vec<ConnectionInfo>,
        details: SignedRelocateDetails,
    ) -> Result<State, RoutingError> {
        Ok(State::BootstrappingPeer(BootstrappingPeer::relocate(
            BootstrappingPeerDetails {
                network_service: self.network_service,
                full_id: self.full_id,
                network_cfg: self.chain.network_cfg(),
                timer: self.timer,
                rng: self.rng,
            },
            conn_infos,
            details,
        )))
    }

    pub fn into_elder(
        self,
        old_pfx: Prefix<XorName>,
        outbox: &mut dyn EventBox,
    ) -> Result<State, RoutingError> {
        let details = ElderDetails {
            chain: self.chain,
            network_service: self.network_service,
            event_backlog: self.event_backlog,
            full_id: self.full_id,
            gen_pfx_info: self.gen_pfx_info,
            routing_msg_queue: Default::default(),
            routing_msg_backlog: self.routing_msg_backlog,
            direct_msg_backlog: self.direct_msg_backlog,
            sig_accumulator: self.sig_accumulator,
            parsec_map: self.parsec_map,
            // we reset the message filter so that the node can correctly process some messages as
            // an Elder even if it has already seen them as an Adult
            routing_msg_filter: RoutingMessageFilter::new(),
            timer: self.timer,
            rng: self.rng,
        };

        Elder::from_adult(details, old_pfx, outbox).map(State::Elder)
    }

    pub fn pause(self) -> PausedState {
        PausedState {
            chain: self.chain,
            full_id: self.full_id,
            gen_pfx_info: self.gen_pfx_info,
            routing_msg_filter: self.routing_msg_filter,
            routing_msg_queue: VecDeque::new(),
            routing_msg_backlog: self.routing_msg_backlog,
            direct_msg_backlog: self.direct_msg_backlog,
            network_service: self.network_service,
            network_rx: None,
            sig_accumulator: self.sig_accumulator,
            parsec_map: self.parsec_map,
        }
    }

    pub fn resume(state: PausedState, timer: Timer) -> Self {
        let knowledge_timer_token = timer.schedule(KNOWLEDGE_TIMEOUT);

        Self {
            chain: state.chain,
            network_service: state.network_service,
            event_backlog: Vec::new(),
            full_id: state.full_id,
            gen_pfx_info: state.gen_pfx_info,
            routing_msg_backlog: state.routing_msg_backlog,
            direct_msg_backlog: state.direct_msg_backlog,
            sig_accumulator: state.sig_accumulator,
            parsec_map: state.parsec_map,
            knowledge_timer_token,
            routing_msg_filter: state.routing_msg_filter,
            timer,
            rng: rng::new(),
        }
    }

    pub fn our_prefix(&self) -> &Prefix<XorName> {
        self.chain.our_prefix()
    }

    fn handle_relocate(&mut self, details: SignedRelocateDetails) -> Transition {
        if details.content().pub_id != *self.id() {
            // This `Relocate` message is not for us - it's most likely a duplicate of a previous
            // message that we already handled.
            return Transition::Stay;
        }

        debug!(
            "{} - Received Relocate message to join the section at {}.",
            self,
            details.content().destination
        );

        if !self.check_signed_relocation_details(&details) {
            return Transition::Stay;
        }

        let conn_infos: Vec<_> = self
            .chain
            .our_elders()
            .map(|p2p_node| p2p_node.connection_info().clone())
            .collect();

        self.network_service_mut().remove_and_disconnect_all();

        Transition::Relocate {
            details,
            conn_infos,
        }
    }

    // Since we are an adult we should only give info about our section elders and they would
    // further guide the joining node.
    // However this lead to a loop if the Adult is the new Elder so we use the same code as
    // in Elder and return Join in some cases.
    fn handle_bootstrap_request(&mut self, p2p_node: P2pNode, destination: XorName) {
        // Use same code as from Elder::respond_to_bootstrap_request.
        // This is problematic since Elders do additional checks before doing this.
        // This was necessary to merge the initial work for promotion demotion.
        let response = if self.our_prefix().matches(&destination) {
            let our_info = self.chain.our_info().clone();
            debug!(
                "{} - Sending BootstrapResponse::Join to {:?} ({:?})",
                self, p2p_node, our_info
            );
            BootstrapResponse::Join(our_info)
        } else {
            let conn_infos: Vec<_> = self
                .closest_known_elders_to(&destination)
                .map(|p2p_node| p2p_node.connection_info().clone())
                .collect();
            debug!(
                "{} - Sending BootstrapResponse::Rebootstrap to {}",
                self, p2p_node
            );
            BootstrapResponse::Rebootstrap(conn_infos)
        };
        self.send_direct_message(
            p2p_node.connection_info(),
            DirectMessage::BootstrapResponse(response),
        );
    }

    fn handle_genesis_update(
        &mut self,
        gen_pfx_info: GenesisPfxInfo,
    ) -> Result<Transition, RoutingError> {
        info!("{} - Received GenesisUpdate: {:?}", self, gen_pfx_info);

        // An Adult can receive the same message from multiple Elders - bail early if we are
        // already up to date
        if gen_pfx_info.parsec_version <= self.gen_pfx_info.parsec_version {
            return Ok(Transition::Stay);
        }
        self.gen_pfx_info = gen_pfx_info.clone();
        self.parsec_map.init(
            &mut self.rng,
            self.full_id.clone(),
            &self.gen_pfx_info,
            &LogIdent::new(self.full_id.public_id()),
        );
        self.chain = Chain::new(self.chain.network_cfg(), *self.id(), gen_pfx_info, None);

        // We were not promoted during the last section change, so we are not going to need these
        // messages anymore. This also prevents the messages from becoming stale (fail the trust
        // check) when they are eventually taken from the backlog and swarmed to other nodes.
        self.routing_msg_backlog.clear();
        self.direct_msg_backlog.clear();

        Ok(Transition::Stay)
    }

    // Send signed_msg to our elders so they can route it properly.
    fn send_signed_message_to_elders(
        &mut self,
        msg: HopMessageWithBytes,
    ) -> Result<(), RoutingError> {
        trace!(
            "{}: Forwarding message {:?} via elder targets {:?}",
            self,
            msg.signed_routing_message(),
            self.chain.our_elders().format(", ")
        );

        let routing_msg_filter = &mut self.routing_msg_filter;
        let targets: Vec<_> = self
            .chain
            .our_elders()
            .filter(|p2p_node| {
                routing_msg_filter
                    .filter_outgoing(&msg, p2p_node.public_id())
                    .is_new()
            })
            .map(|node| node.connection_info().clone())
            .collect();

        let cheap_bytes_clone = msg.full_message_bytes().clone();
        self.send_message_to_targets(&targets, targets.len(), cheap_bytes_clone);

        // we've seen this message - don't handle it again if someone else sends it to us
        let _ = self.routing_msg_filter.filter_incoming(&msg);

        Ok(())
    }

    /// Handles a signature of a `SignedMessage`, and if we have enough to verify the signed
    /// message, handles it.
    fn handle_message_signature(
        &mut self,
        msg: SignedRoutingMessage,
        pub_id: PublicId,
    ) -> Result<Transition, RoutingError> {
        if !self.chain.is_peer_elder(&pub_id) {
            debug!(
                "{} - Received message signature from not known elder (still use it) {}, {:?}",
                self, pub_id, msg
            );
        }

        if let Some(signed_msg) = self.sig_accumulator.add_proof(msg) {
            let signed_msg = HopMessageWithBytes::new(signed_msg)?;
            self.handle_signed_message(signed_msg)
        } else {
            Ok(Transition::Stay)
        }
    }

    // If the message is for us, verify it then, handle the enclosed routing message and swarm it
    // to the rest of our section when destination is targeting multiple; if not, forward it.
    fn handle_signed_message(
        &mut self,
        msg: HopMessageWithBytes,
    ) -> Result<Transition, RoutingError> {
        if !self.routing_msg_filter.filter_incoming(&msg).is_new() {
            trace!(
                "{} Known message: {:?} - not handling further",
                self,
                msg.routing_message()
            );
            return Ok(Transition::Stay);
        }

        self.handle_filtered_signed_message(msg)
    }

    fn handle_filtered_signed_message(
        &mut self,
        msg: HopMessageWithBytes,
    ) -> Result<Transition, RoutingError> {
        trace!(
            "{} - Handle signed message: {:?}",
            self,
            msg.routing_message()
        );

        if self.in_location(msg.message_dst()) {
            let signed_msg = msg.signed_routing_message();
            match &signed_msg.routing_message().content {
                MessageContent::GenesisUpdate(info) => {
                    self.verify_signed_message(signed_msg)?;
                    return self.handle_genesis_update(info.clone());
                }
                _ => {
                    self.routing_msg_backlog.push(signed_msg.clone());
                }
            }
        }

        self.send_signed_message_to_elders(msg)?;
        Ok(Transition::Stay)
    }

    fn verify_signed_message(&self, msg: &SignedRoutingMessage) -> Result<(), RoutingError> {
        let result = match msg.verify(self.chain.get_their_keys_info()) {
            Ok(VerifyStatus::Full) => Ok(()),
            Ok(VerifyStatus::ProofTooNew) => Err(RoutingError::UntrustedMessage),
            Err(error) => Err(error),
        };
        result.map_err(|error| {
            self.log_verify_failure(msg, &error);
            error
        })
    }
}

#[cfg(feature = "mock_base")]
impl Adult {
    pub fn chain(&self) -> &Chain {
        &self.chain
    }

    pub fn process_timers(&mut self) {
        self.timer.process_timers()
    }

    pub fn has_unpolled_observations(&self) -> bool {
        self.parsec_map.has_unpolled_observations()
    }

    pub fn unpolled_observations_string(&self) -> String {
        self.parsec_map.unpolled_observations_string()
    }
}

impl Base for Adult {
    fn network_service(&self) -> &NetworkService {
        &self.network_service
    }

    fn network_service_mut(&mut self) -> &mut NetworkService {
        &mut self.network_service
    }

    fn full_id(&self) -> &FullId {
        &self.full_id
    }

    fn in_location(&self, auth: &Location) -> bool {
        self.chain.in_location(auth)
    }

    fn peer_map(&self) -> &PeerMap {
        &self.network_service().peer_map
    }

    fn peer_map_mut(&mut self) -> &mut PeerMap {
        &mut self.network_service_mut().peer_map
    }

    fn timer(&mut self) -> &mut Timer {
        &mut self.timer
    }

    fn rng(&mut self) -> &mut MainRng {
        &mut self.rng
    }

    fn finish_handle_transition(&mut self, outbox: &mut dyn EventBox) -> Transition {
        debug!("{} - State changed to Adult finished.", self);

        let mut transition = Transition::Stay;

        for (pub_id, msg) in mem::replace(&mut self.direct_msg_backlog, Default::default()) {
            if let Transition::Stay = &transition {
                match self.handle_direct_message(msg, pub_id, outbox) {
                    Ok(new_transition) => transition = new_transition,
                    Err(err) => debug!("{} - {:?}", self, err),
                }
            } else {
                self.direct_msg_backlog.push((pub_id, msg));
            }
        }

        for msg in mem::replace(&mut self.routing_msg_backlog, Default::default()) {
            if let Transition::Stay = &transition {
                let msg = match HopMessageWithBytes::new(msg) {
                    Ok(msg) => msg,
                    Err(err) => {
                        error!("{} - Failed to make message {:?}", self, err);
                        continue;
                    }
                };

                match self.handle_filtered_signed_message(msg) {
                    Ok(new_transition) => transition = new_transition,
                    Err(err) => debug!("{} - {:?}", self, err),
                }
            } else {
                self.routing_msg_backlog.push(msg);
            }
        }

        transition
    }

    fn handle_timeout(&mut self, token: u64, _: &mut dyn EventBox) -> Transition {
        if self.knowledge_timer_token == token {
            // TODO: send this only when the knowledge changes, not periodically.
            self.send_member_knowledge();
            self.knowledge_timer_token = self.timer.schedule(KNOWLEDGE_TIMEOUT);
        }

        Transition::Stay
    }

    fn handle_peer_lost(&mut self, peer_addr: SocketAddr, _: &mut dyn EventBox) -> Transition {
        debug!("{} - Lost peer {}", self, peer_addr);
        Transition::Stay
    }

    fn handle_direct_message(
        &mut self,
        msg: DirectMessage,
        p2p_node: P2pNode,
        outbox: &mut dyn EventBox,
    ) -> Result<Transition, RoutingError> {
        use crate::messages::DirectMessage::*;
        match msg {
            MessageSignature(msg) => self.handle_message_signature(*msg, *p2p_node.public_id()),
            ParsecRequest(version, par_request) => {
                self.handle_parsec_request(version, par_request, p2p_node, outbox)
            }
            ParsecResponse(version, par_response) => {
                self.handle_parsec_response(version, par_response, *p2p_node.public_id(), outbox)
            }
            BootstrapRequest(name) => {
                self.handle_bootstrap_request(p2p_node, name);
                Ok(Transition::Stay)
            }
            ConnectionResponse => {
                debug!("{} - Received connection response from {}", self, p2p_node);
                Ok(Transition::Stay)
            }
            Relocate(details) => Ok(self.handle_relocate(*details)),
            msg @ BootstrapResponse(_) => {
                debug!(
                    "{} Unhandled direct message from {}, discard: {:?}",
                    self,
                    p2p_node.public_id(),
                    msg
                );
                Ok(Transition::Stay)
            }
            msg @ JoinRequest(_) | msg @ MemberKnowledge { .. } => {
                debug!(
                    "{} Unhandled direct message from {}, adding to backlog: {:?}",
                    self,
                    p2p_node.public_id(),
                    msg
                );
                self.direct_msg_backlog.push((p2p_node, msg));
                Ok(Transition::Stay)
            }
        }
    }

    fn handle_hop_message(
        &mut self,
        msg: HopMessageWithBytes,
        _outbox: &mut dyn EventBox,
    ) -> Result<Transition, RoutingError> {
        self.handle_signed_message(msg)
    }
}

impl Approved for Adult {
    fn send_event(&mut self, event: Event, _: &mut dyn EventBox) {
        self.event_backlog.push(event)
    }

    fn parsec_map(&self) -> &ParsecMap {
        &self.parsec_map
    }

    fn parsec_map_mut(&mut self) -> &mut ParsecMap {
        &mut self.parsec_map
    }

    fn chain(&self) -> &Chain {
        &self.chain
    }

    fn chain_mut(&mut self) -> &mut Chain {
        &mut self.chain
    }

    fn set_pfx_successfully_polled(&mut self, _: bool) {
        // Doesn't do anything
    }

    fn is_pfx_successfully_polled(&self) -> bool {
        false
    }

    fn handle_relocate_polled(&mut self, _details: RelocateDetails) -> Result<(), RoutingError> {
        Ok(())
    }

    fn handle_promote_and_demote_elders(
        &mut self,
        _new_infos: Vec<EldersInfo>,
    ) -> Result<(), RoutingError> {
        Ok(())
    }

    fn handle_member_added(
        &mut self,
        _payload: OnlinePayload,
        _outbox: &mut dyn EventBox,
    ) -> Result<(), RoutingError> {
        Ok(())
    }

    fn handle_member_removed(
        &mut self,
        _pub_id: PublicId,
        _outbox: &mut dyn EventBox,
    ) -> Result<(), RoutingError> {
        Ok(())
    }

    fn handle_member_relocated(
        &mut self,
        _details: RelocateDetails,
        _signature: bls::Signature,
        _node_knowledge: u64,
        _outbox: &mut dyn EventBox,
    ) {
    }

    fn handle_dkg_result_event(
        &mut self,
        _participants: &BTreeSet<PublicId>,
        _dkg_result: &DkgResultWrapper,
    ) -> Result<(), RoutingError> {
        // TODO
        Ok(())
    }

    fn handle_section_info_event(
        &mut self,
        old_pfx: Prefix<XorName>,
        _neighbour_change: EldersChange,
        _: &mut dyn EventBox,
    ) -> Result<Transition, RoutingError> {
        if self.chain.is_self_elder() {
            Ok(Transition::IntoElder { old_pfx })
        } else {
            debug!("{} - Unhandled SectionInfo event", self);
            Ok(Transition::Stay)
        }
    }

    fn handle_neighbour_info_event(
        &mut self,
        _elders_info: EldersInfo,
        _neighbour_change: EldersChange,
    ) -> Result<(), RoutingError> {
        Ok(())
    }

    fn handle_relocate_prepare_event(
        &mut self,
        _payload: RelocateDetails,
        _count_down: i32,
        _outbox: &mut dyn EventBox,
    ) -> Result<(), RoutingError> {
        Ok(())
    }

    fn handle_their_key_info_event(
        &mut self,
        _key_info: SectionKeyInfo,
    ) -> Result<(), RoutingError> {
        Ok(())
    }

    fn handle_send_ack_message_event(
        &mut self,
        _ack_payload: SendAckMessagePayload,
    ) -> Result<(), RoutingError> {
        Ok(())
    }

    fn handle_prune_event(&mut self) -> Result<(), RoutingError> {
        debug!("{} - Unhandled ParsecPrune event", self);
        Ok(())
    }
}

impl Display for Adult {
    fn fmt(&self, formatter: &mut Formatter) -> fmt::Result {
        write!(formatter, "Adult({}({:b}))", self.name(), self.our_prefix())
    }
}