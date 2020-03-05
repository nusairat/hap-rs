use std::{collections::HashMap, str};

use chacha20_poly1305_aead;
use crypto::{curve25519, ed25519};
use futures::sync::oneshot;
use log::{debug,warn};
use rand::{self, Rng};
use ring::{digest, hkdf, hmac};
use uuid::Uuid;

use crate::{
    config::ConfigPtr,
    db::DatabasePtr,
    event::EventEmitterPtr,
    protocol::{
        tlv::{self, Type, Value},
        Device,
        IdPtr,
        Pairing,
    },
    transport::{http::handler::TlvHandler, tcp},
};

struct Session {
    b_pub: [u8; 32],
    a_pub: Vec<u8>,
    shared_secret: [u8; 32],
    session_key: Vec<u8>,
}

pub struct PairVerify {
    session: Option<Session>,
    session_sender: Option<oneshot::Sender<tcp::Session>>,
}

impl PairVerify {
    pub fn new(session_sender: oneshot::Sender<tcp::Session>) -> PairVerify {
        PairVerify {
            session: None,
            session_sender: Some(session_sender),
        }
    }
}

enum StepNumber {
    Unknown = 0,
    StartReq = 1,
    StartRes = 2,
    FinishReq = 3,
    FinishRes = 4,
}

pub enum Step {
    Start { a_pub: Vec<u8> },
    Finish { data: Vec<u8> },
}

impl TlvHandler for PairVerify {
    type ParseResult = Step;
    type Result = tlv::Container;

    fn parse(&self, body: Vec<u8>) -> Result<Step, tlv::ErrorContainer> {
        let decoded = tlv::decode(body);
        match decoded.get(&(Type::State as u8)) {
            Some(method) => match method[0] {
                x if x == StepNumber::StartReq as u8 => {
                    let a_pub = decoded.get(&(Type::PublicKey as u8)).ok_or(tlv::ErrorContainer::new(
                        StepNumber::StartRes as u8,
                        tlv::Error::Unknown,
                    ))?;
                    Ok(Step::Start { a_pub: a_pub.clone() })
                },
                x if x == StepNumber::FinishReq as u8 => {
                    let data = decoded
                        .get(&(Type::EncryptedData as u8))
                        .ok_or(tlv::ErrorContainer::new(
                            StepNumber::FinishRes as u8,
                            tlv::Error::Unknown,
                        ))?;
                    Ok(Step::Finish { data: data.clone() })
                },
                _ => Err(tlv::ErrorContainer::new(StepNumber::Unknown as u8, tlv::Error::Unknown)),
            },
            None => Err(tlv::ErrorContainer::new(StepNumber::Unknown as u8, tlv::Error::Unknown)),
        }
    }

    fn handle(
        &mut self,
        step: Step,
        _: &IdPtr,
        _: &ConfigPtr,
        database: &DatabasePtr,
        _: &EventEmitterPtr,
    ) -> Result<tlv::Container, tlv::ErrorContainer> {
        match step {
            Step::Start { a_pub } => match handle_start(self, database, a_pub) {
                Ok(res) => Ok(res),
                Err(err) => Err(tlv::ErrorContainer::new(StepNumber::StartRes as u8, err)),
            },
            Step::Finish { data } => match handle_finish(self, database, &data) {
                Ok(res) => Ok(res),
                Err(err) => Err(tlv::ErrorContainer::new(StepNumber::FinishRes as u8, err)),
            },
        }
    }
}

fn handle_start(
    handler: &mut PairVerify,
    database: &DatabasePtr,
    a_pub: Vec<u8>,
) -> Result<tlv::Container, tlv::Error> {
    use super::pair_setup::{PayloadU8,PayloadU8Len};

    debug!("M1: Got Verify Start Request");

    let mut rng = rand::thread_rng();
    let b = rng.gen::<[u8; 32]>();
    let b_pub = curve25519::curve25519_base(&b);
    let shared_secret = curve25519::curve25519(&b, &a_pub);

    let accessory = Device::load_from(database)?;
    let mut accessory_info: Vec<u8> = Vec::new();
    accessory_info.extend(&b_pub);
    accessory_info.extend(accessory.id.as_bytes());
    accessory_info.extend(&a_pub);
    let accessory_signature = ed25519::signature(&accessory_info, &accessory.private_key);

    let mut sub_tlv: HashMap<u8, Vec<u8>> = HashMap::new();
    let (t, v) = Value::Identifier(accessory.id).as_tlv();
    sub_tlv.insert(t, v);
    let (t, v) = Value::Signature(accessory_signature.to_vec()).as_tlv();
    sub_tlv.insert(t, v);
    let encoded_sub_tlv = tlv::encode(sub_tlv);

    // let mut session_key = [0; 32];
    // let salt = hmac::SigningKey::new(&digest::SHA512, b"Pair-Verify-Encrypt-Salt");
    // hkdf::extract_and_expand(&salt, &shared_secret, b"Pair-Verify-Encrypt-Info", &mut session_key);

    let mut session_key = [0; 32];
    let salt = hkdf::Salt::new(hkdf::HKDF_SHA512, b"Pair-Verify-Encrypt-Salt");
    let payload = PayloadU8Len(session_key.len());
    let PayloadU8(session_key) = salt.extract(&shared_secret)
                .expand(&[b"Pair-Verify-Encrypt-Info"], payload)
                .unwrap()
                .into();
//    let session_key2 :  &[u8] = out.as_ref();

    handler.session = Some(Session {
        b_pub: b_pub,
        a_pub: a_pub,
        shared_secret: shared_secret,
        session_key: session_key.clone(),
    });

    let mut encrypted_data = Vec::new();
    let mut nonce = vec![0; 4];
    nonce.extend(b"PV-Msg02");
    let auth_tag = chacha20_poly1305_aead::encrypt(&session_key, &nonce, &[], &encoded_sub_tlv, &mut encrypted_data)?;
    encrypted_data.extend(&auth_tag);

    debug!("M2: Sending Verify Start Response");

    Ok(vec![
        Value::State(StepNumber::StartRes as u8),
        Value::PublicKey(b_pub.to_vec()),
        Value::EncryptedData(encrypted_data),
    ])
}

fn handle_finish(handler: &mut PairVerify, database: &DatabasePtr, data: &[u8]) -> Result<tlv::Container, tlv::Error> {
    debug!("M3: Got Verify Finish Request-");

    if let Some(ref mut session) = handler.session {
        let encrypted_data = Vec::from(&data[..data.len() - 16]);
        let auth_tag = Vec::from(&data[data.len() - 16..]);

        let mut decrypted_data = Vec::new();
        let mut nonce = vec![0; 4];
        nonce.extend(b"PV-Msg03");
        chacha20_poly1305_aead::decrypt(
            &session.session_key,
            &nonce,
            &[],
            &encrypted_data,
            &auth_tag,
            &mut decrypted_data,
        )?;

        let sub_tlv = tlv::decode(decrypted_data);
        let device_pairing_id = sub_tlv.get(&(Type::Identifier as u8)).ok_or(tlv::Error::Unknown)?;
        let device_signature = sub_tlv.get(&(Type::Signature as u8)).ok_or(tlv::Error::Unknown)?;

        let uuid_str = str::from_utf8(device_pairing_id)?;
        let pairing_uuid = Uuid::parse_str(uuid_str)?;
        let pairing = Pairing::load_from(pairing_uuid, database)?;
        let mut device_info: Vec<u8> = Vec::new();
        device_info.extend(&session.a_pub);
        device_info.extend(device_pairing_id);
        device_info.extend(&session.b_pub);
        if !ed25519::verify(&device_info, &pairing.public_key, &device_signature) {
            return Err(tlv::Error::Authentication);
        }

        if let Some(sender) = handler.session_sender.take() {
            let encrypted_session = tcp::Session {
                controller_id: pairing_uuid,
                shared_secret: session.shared_secret,
            };
            let _session = sender.send(encrypted_session);
        } else {
            return Err(tlv::Error::Unknown);
        }

        debug!("M4: Sending Verify Finish Response");

        Ok(vec![Value::State(StepNumber::FinishRes as u8)])
    } else {
        warn!("M3 Error");
        Err(tlv::Error::Unknown)
    }
}
