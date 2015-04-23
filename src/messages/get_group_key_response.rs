// Copyright 2015 MaidSafe.net limited
//
// This MaidSafe Software is licensed to you under (1) the MaidSafe.net Commercial License,
// version 1.0 or later, or (2) The General Public License (GPL), version 3, depending on which
// licence you accepted on initial access to the Software (the "Licences").
//
// By contributing code to the MaidSafe Software, or to this project generally, you agree to be
// bound by the terms of the MaidSafe Contributor Agreement, version 1.0, found in the root
// directory of this project at LICENSE, COPYING and CONTRIBUTOR respectively and also
// available at: http://www.maidsafe.net/licenses
//
// Unless required by applicable law or agreed to in writing, the MaidSafe Software distributed
// under the GPL Licence is distributed on an "AS IS" BASIS, WITHOUT WARRANTIES OR CONDITIONS
// OF ANY KIND, either express or implied.
//
// See the Licences for the specific language governing permissions and limitations relating to
// use of the MaidSafe Software.

#![allow(unused_assignments)]

use cbor::CborTagEncode;
use rustc_serialize::{Decodable, Decoder, Encodable, Encoder};
use histogram::{Histogram};

use types;

#[derive(PartialEq, Eq, PartialOrd, Ord, Clone, Debug)]
pub struct GetGroupKeyResponse {
  pub target_id : types::GroupAddress,
  pub public_sign_keys : Vec<(types::DhtId, types::PublicSignKey)>
}

impl GetGroupKeyResponse {
    pub fn generate_random() -> GetGroupKeyResponse {
        let total: usize = types::GROUP_SIZE as usize + 7;
        let mut vec: Vec<(types::DhtId, types::PublicSignKey)> = Vec::with_capacity(total);
        for i in 0..total {
            vec.push((types::DhtId::generate_random(), types::PublicSignKey::generate_random()));
        }
        GetGroupKeyResponse {
            target_id: types::DhtId::generate_random(),
            public_sign_keys: vec,
        }
    }

    pub fn merge(&self, get_group_key_responses: &Vec<GetGroupKeyResponse>) -> Option<GetGroupKeyResponse> {
      type Key = (types::DhtId, types::PublicSignKey);

      let mut histogram = Histogram::new();

      for public_sign_key in &self.public_sign_keys {
          histogram.update(public_sign_key.clone());
      }

      for other in get_group_key_responses {
        if other.target_id != self.target_id { return None; }
        for public_sign_key in &other.public_sign_keys {
            histogram.update(public_sign_key.clone());
        }
      }

      let mut merged_group = Vec::<Key>::with_capacity(types::GROUP_SIZE as usize);

      for public_sign_key in histogram.sort_by_highest() {
        if merged_group.len() < types::GROUP_SIZE as usize {
          // can also be done with map_in_place,
          // but explicit for-loop allows for asserts
          // assert!(public_pmid_count.1 >= types::QUORUM_SIZE as usize);
          assert!(public_sign_key.1 <= types::GROUP_SIZE as usize);
          // TODO(ben 2015-04-09) return None once logic assured
          merged_group.push(public_sign_key.0);
        } else {
          break; //  NOTE(ben 2015-04-15): here we can measure the fuzzy
                 //  boundary of groups
        }
      }
      assert_eq!(merged_group.len(), types::GROUP_SIZE as usize);
      // TODO(ben 2015-04-09) : curtosy call to sort to target,
      //                        but requires correct name on PublicPmid
      // merged_group.sort_by(...)
      Some(GetGroupKeyResponse{target_id : self.target_id.clone(),
                             public_sign_keys : merged_group})
    }
}

impl Encodable for GetGroupKeyResponse {
  fn encode<E: Encoder>(&self, e: &mut E)->Result<(), E::Error> {
    CborTagEncode::new(5483_001, &(&self.target_id, &self.public_sign_keys)).encode(e)
  }
}

impl Decodable for GetGroupKeyResponse {
  fn decode<D: Decoder>(d: &mut D)->Result<GetGroupKeyResponse, D::Error> {
    try!(d.read_u64());
    let (target_id, public_sign_keys) = try!(Decodable::decode(d));
    Ok(GetGroupKeyResponse { target_id: target_id , public_sign_keys: public_sign_keys})
  }
}

#[cfg(test)]
mod test {
    use types;
    use super::*;
    use cbor; 
    
    #[test]
    fn get_group_key_response_serialisation() {
        let obj_before = GetGroupKeyResponse::generate_random();

        let mut e = cbor::Encoder::from_memory();
        e.encode(&[&obj_before]).unwrap();

        let mut d = cbor::Decoder::from_bytes(e.as_bytes());
        let obj_after: GetGroupKeyResponse = d.decode().next().unwrap().unwrap();

        assert_eq!(obj_before, obj_after);
    }

    #[test]
    fn merge() {
        let obj = GetGroupKeyResponse::generate_random();
        assert!(obj.public_sign_keys.len() >= types::GROUP_SIZE as usize);
        // if group size changes, reimplement the below
        assert!(types::GROUP_SIZE >= 13);

        // pick random keys
        let mut keys = Vec::<(types::DhtId, types::PublicSignKey)>::with_capacity(7);
        keys.push(obj.public_sign_keys[3].clone());
        keys.push(obj.public_sign_keys[5].clone());
        keys.push(obj.public_sign_keys[7].clone());
        keys.push(obj.public_sign_keys[8].clone());
        keys.push(obj.public_sign_keys[9].clone());
        keys.push(obj.public_sign_keys[10].clone());
        keys.push(obj.public_sign_keys[13].clone());

        let mut responses = Vec::<GetGroupKeyResponse>::with_capacity(4);
        for _ in 0..4 {
            let mut response = GetGroupKeyResponse::generate_random();
            response.target_id = obj.target_id.clone();
            response.public_sign_keys[1] = keys[0].clone();
            response.public_sign_keys[4] = keys[1].clone();
            response.public_sign_keys[6] = keys[2].clone();
            response.public_sign_keys[0] = keys[3].clone();
            response.public_sign_keys[5] = keys[4].clone();
            response.public_sign_keys[9] = keys[5].clone();
            response.public_sign_keys[10] = keys[6].clone();
            responses.push(response);
        }

        let merged_obj = obj.merge(&responses);
        assert!(merged_obj.is_some());
        let merged_response = merged_obj.unwrap();
        for i in 0..7 {
            assert!(keys.iter().find(|a| **a == merged_response.public_sign_keys[i]).is_some());
        }
    }
}
