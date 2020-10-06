// Copyright 2020 MaidSafe.net limited.
//
// This SAFE Network Software is licensed to you under The General Public License (GPL), version 3.
// Unless required by applicable law or agreed to in writing, the SAFE Network Software distributed
// under the GPL Licence is distributed on an "AS IS" BASIS, WITHOUT WARRANTIES OR CONDITIONS OF ANY
// KIND, either express or implied. Please review the Licences for the specific language governing
// permissions and limitations relating to use of the SAFE Network Software.

//! Utilities for sn_routing messages through the network.

use crate::{
    error::{Error, Result},
    location::DstLocation,
    peer::Peer,
    section::{SectionMap, SectionMembers},
};

use itertools::Itertools;
use xor_name::XorName;

/// Returns the delivery group size based on the section size `n`
pub const fn delivery_group_size(n: usize) -> usize {
    // this is an integer that is ≥ n/3
    (n + 2) / 3
}

/// Returns a set of nodes to which a message for the given `DstLocation` could be sent
/// onwards, sorted by priority, along with the number of targets the message should be sent to.
/// If the total number of targets returned is larger than this number, the spare targets can
/// be used if the message can't be delivered to some of the initial ones.
///
/// * If the destination is an `DstLocation::Section`:
///     - if our section is the closest on the network (i.e. our section's prefix is a prefix of
///       the destination), returns all other members of our section; otherwise
///     - returns the `N/3` closest members to the target
///
/// * If the destination is an individual node:
///     - if our name *is* the destination, returns an empty set; otherwise
///     - if the destination name is an entry in the routing table, returns it; otherwise
///     - returns the `N/3` closest members of the RT to the target
pub fn delivery_targets(
    dst: &DstLocation,
    our_id: &XorName,
    our_members: &SectionMembers,
    sections: &SectionMap,
) -> Result<(Vec<Peer>, usize)> {
    if !sections.is_elder(our_id) {
        // We are not Elder - return all the elders of our section, so the message can be properly
        // relayed through them.
        let targets: Vec<_> = sections.our_elders().cloned().collect();
        let dg_size = targets.len();
        return Ok((targets, dg_size));
    }

    let (best_section, dg_size) = match dst {
        DstLocation::Node(target_name) => {
            if target_name == our_id {
                return Ok((Vec::new(), 0));
            }
            if let Some(node) = get_peer(target_name, our_members, sections) {
                return Ok((vec![*node], 1));
            }

            candidates(target_name, our_id, sections)?
        }
        DstLocation::Section(target_name) => {
            let info = sections.closest(target_name);
            if info.prefix == sections.our().prefix
                || info.prefix.is_neighbour(&sections.our().prefix)
            {
                // Exclude our name since we don't need to send to ourself

                // FIXME: only doing this for now to match RT.
                // should confirm if needed esp after msg_relay changes.
                let section: Vec<_> = info
                    .elders
                    .values()
                    .filter(|node| node.name() != our_id)
                    .cloned()
                    .collect();
                let dg_size = section.len();
                return Ok((section, dg_size));
            }

            candidates(target_name, our_id, sections)?
        }
        DstLocation::Direct => return Err(Error::CannotRoute),
    };

    Ok((best_section, dg_size))
}

// Obtain the delivery group candidates for this target
fn candidates(
    target_name: &XorName,
    our_id: &XorName,
    sections: &SectionMap,
) -> Result<(Vec<Peer>, usize)> {
    let filtered_sections = sections
        .sorted_by_distance_to(target_name)
        .into_iter()
        .map(|info| (&info.prefix, info.elders.len(), info.elders.values()));

    let mut dg_size = 0;
    let mut nodes_to_send = Vec::new();
    for (idx, (prefix, len, connected)) in filtered_sections.enumerate() {
        nodes_to_send.extend(connected.cloned());
        dg_size = delivery_group_size(len);

        if *prefix == sections.our().prefix {
            // Send to all connected targets so they can forward the message
            nodes_to_send.retain(|node| node.name() != our_id);
            dg_size = nodes_to_send.len();
            break;
        }
        if idx == 0 && nodes_to_send.len() >= dg_size {
            // can deliver to enough of the closest section
            break;
        }
    }
    nodes_to_send.sort_by(|lhs, rhs| target_name.cmp_distance(lhs.name(), rhs.name()));

    if dg_size > 0 && nodes_to_send.len() >= dg_size {
        Ok((nodes_to_send, dg_size))
    } else {
        Err(Error::CannotRoute)
    }
}

// Returns a `Peer` for a known node.
fn get_peer<'a>(
    name: &XorName,
    our_members: &'a SectionMembers,
    sections: &'a SectionMap,
) -> Option<&'a Peer> {
    our_members
        .get(name)
        .map(|info| &info.peer)
        .or_else(|| sections.get_elder(name))
}

// Returns the set of peers that are responsible for collecting signatures to verify a message;
// this may contain us or only other nodes.
pub fn signature_targets<I>(dst: &DstLocation, our_elders: I) -> Vec<Peer>
where
    I: IntoIterator<Item = Peer>,
{
    let dst_name = match dst {
        DstLocation::Node(name) => *name,
        DstLocation::Section(name) => *name,
        DstLocation::Direct => {
            error!("Invalid destination for signature targets: {:?}", dst);
            return vec![];
        }
    };

    let mut list: Vec<_> = our_elders
        .into_iter()
        .sorted_by(|lhs, rhs| dst_name.cmp_distance(lhs.name(), rhs.name()))
        .collect();
    list.truncate(delivery_group_size(list.len()));
    list
}