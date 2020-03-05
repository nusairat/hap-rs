use std::{collections::HashMap, ops::BitXor, str};

use chacha20_poly1305_aead;
use crypto::ed25519;
use log::{debug,warn};
use num::BigUint;
use rand::{self, distributions::Standard, Rng};
use ring::{digest, hkdf, hmac};
use sha2::{Digest, Sha512};
use srp::{
    client::{srp_private_key, SrpClient},
    groups::G_3072,
    server::{SrpServer, UserRecord},
    types::SrpGroup,
};
use uuid::Uuid;

use crate::{
    config::ConfigPtr,
    db::DatabasePtr,
    event::{Event, EventEmitterPtr},
    protocol::{
        tlv::{self, Type, Value},
        Device,
        IdPtr,
        Pairing,
        Permissions,
    },
    transport::http::handler::TlvHandler,
};
use ring::hkdf::Algorithm;

struct Session {
    salt: Vec<u8>,
    verifier: Vec<u8>,
    b: Vec<u8>,
    b_pub: Vec<u8>,
    shared_secret: Option<Vec<u8>>,
}

pub struct PairSetup {
    session: Option<Session>,
    unsuccessful_tries: u8,
}

impl PairSetup {
    pub fn new() -> PairSetup {
        PairSetup {
            session: None,
            unsuccessful_tries: 0,
        }
    }
}

enum StepNumber {
    Unknown = 0,
    StartReq = 1,
    StartRes = 2,
    VerifyReq = 3,
    VerifyRes = 4,
    ExchangeReq = 5,
    ExchangeRes = 6,
}

pub enum Step {
    Start,
    Verify { a_pub: Vec<u8>, a_proof: Vec<u8> },
    Exchange { data: Vec<u8> },
}

impl TlvHandler for PairSetup {
    type ParseResult = Step;
    type Result = tlv::Container;

    fn parse(&self, body: Vec<u8>) -> Result<Step, tlv::ErrorContainer> {
        let mut decoded = tlv::decode(body);
        match decoded.get(&(Type::State as u8)) {
            Some(method) => match method[0] {
                x if x == StepNumber::StartReq as u8 => Ok(Step::Start),
                x if x == StepNumber::VerifyReq as u8 => {
                    let a_pub = decoded
                        .remove(&(Type::PublicKey as u8))
                        .ok_or(tlv::ErrorContainer::new(
                            StepNumber::VerifyRes as u8,
                            tlv::Error::Unknown,
                        ))?;
                    let a_proof = decoded.remove(&(Type::Proof as u8)).ok_or(tlv::ErrorContainer::new(
                        StepNumber::VerifyRes as u8,
                        tlv::Error::Unknown,
                    ))?;
                    Ok(Step::Verify { a_pub, a_proof })
                },
                x if x == StepNumber::ExchangeReq as u8 => {
                    let data = decoded
                        .remove(&(Type::EncryptedData as u8))
                        .ok_or(tlv::ErrorContainer::new(
                            StepNumber::ExchangeRes as u8,
                            tlv::Error::Unknown,
                        ))?;
                    Ok(Step::Exchange { data })
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
        config: &ConfigPtr,
        database: &DatabasePtr,
        event_emitter: &EventEmitterPtr,
    ) -> Result<tlv::Container, tlv::ErrorContainer> {
        match step {
            Step::Start => match handle_start(self, database) {
                Ok(res) => {
                    self.unsuccessful_tries = 0;
                    Ok(res)
                },
                Err(err) => {
                    warn!("Error start");
                    self.unsuccessful_tries += 1;
                    Err(tlv::ErrorContainer::new(StepNumber::StartRes as u8, err))
                },
            },
            Step::Verify { a_pub, a_proof } => match handle_verify(self, &a_pub, &a_proof) {
                Ok(res) => {
                    self.unsuccessful_tries = 0;
                    Ok(res)
                },
                Err(err) => {
                    warn!("Error Verify Step");
                    self.unsuccessful_tries += 1;
                    Err(tlv::ErrorContainer::new(StepNumber::VerifyRes as u8, err))
                },
            },
            Step::Exchange { data } => match handle_exchange(self, config, database, event_emitter, &data) {
                Ok(res) => {
                    debug!("Step Exchange");
                    self.unsuccessful_tries = 0;
                    Ok(res)
                },
                Err(err) => {
                    warn!("Error Exchange");
                    self.unsuccessful_tries += 1;
                    Err(tlv::ErrorContainer::new(StepNumber::ExchangeRes as u8, err))
                },
            },
        }
    }
}

fn handle_start(handler: &mut PairSetup, database: &DatabasePtr) -> Result<tlv::Container, tlv::Error> {
    debug!("M1: Got SRP Start Request");

    if handler.unsuccessful_tries > 99 {
        return Err(tlv::Error::MaxTries);
    }

    let accessory = Device::load_from(database)?;

    let rng = rand::thread_rng();
    let salt = rng.sample_iter::<u8, Standard>(Standard).take(16).collect::<Vec<u8>>(); // s
    let b = rng.sample_iter::<u8, Standard>(Standard).take(64).collect::<Vec<u8>>();

    let private_key = srp_private_key::<Sha512>(b"Pair-Setup", accessory.pin.as_bytes(), &salt); // x = H(s | H(I | ":" | P))
    let srp_client = SrpClient::<Sha512>::new(&private_key, &G_3072);
    let verifier = srp_client.get_password_verifier(&private_key); // v = g^x

    let user = UserRecord {
        username: b"Pair-Setup",
        salt: &salt,
        verifier: &verifier,
    };
    let srp_server = SrpServer::<Sha512>::new(&user, b"foo", &b, &G_3072)?;
    let b_pub = srp_server.get_b_pub();

    handler.session = Some(Session {
        salt: salt.clone(),
        verifier: verifier.clone(),
        b: b.clone(),
        b_pub: b_pub.clone(),
        shared_secret: None,
    });

    debug!("M2: Sending SRP Start Response");

    Ok(vec![
        Value::State(StepNumber::StartRes as u8),
        Value::PublicKey(b_pub),
        Value::Salt(salt.clone()),
    ])
}

fn handle_verify(handler: &mut PairSetup, a_pub: &[u8], a_proof: &[u8]) -> Result<tlv::Container, tlv::Error> {
    debug!("M3: Got SRP Verify Request");

    if let Some(ref mut session) = handler.session {
        let user = UserRecord {
            username: b"Pair-Setup",
            salt: &session.salt,
            verifier: &session.verifier,
        };
        let srp_server = SrpServer::<Sha512>::new(&user, &a_pub, &session.b, &G_3072)?;
        let shared_secret = srp_server.get_key();
        session.shared_secret = Some(shared_secret.as_slice().to_vec());
        let b_proof = verify_client_proof::<Sha512>(
            &session.b_pub,
            a_pub,
            a_proof,
            &session.salt,
            &shared_secret.as_slice().to_vec(),
            &G_3072,
        )?;

        debug!("M4: Sending SRP Verify Response");

        Ok(vec![Value::State(StepNumber::VerifyRes as u8), Value::Proof(b_proof)])
    } else {
        warn!("Error M3");
        Err(tlv::Error::Unknown)
    }
}

fn handle_exchange(
    handler: &mut PairSetup,
    config: &ConfigPtr,
    database: &DatabasePtr,
    event_emitter: &EventEmitterPtr,
    data: &[u8],
) -> Result<tlv::Container, tlv::Error> {
    use ring::hkdf::KeyType;
    // use ring::hmac::KeyType;
    use ring::{digest, hmac, rand};

    debug!("M5: Got SRP Exchange Request");

    if let Some(ref mut session) = handler.session {
        if let Some(ref mut shared_secret) = session.shared_secret {
            let encrypted_data = Vec::from(&data[..data.len() - 16]);
            let auth_tag = Vec::from(&data[data.len() - 16..]);

            let mut encryption_key = [0; 32];
            let salt = hkdf::Salt::new(hkdf::HKDF_SHA512, b"Pair-Setup-Encrypt-Salt");
            let payload = PayloadU8Len(encryption_key.len());
            let info = b"Pair-Setup-Encrypt-Info";
            let PayloadU8(encryption_key) = salt.extract(&shared_secret)
                        .expand(&[info], payload)
                        .unwrap()
                        .into();

            let mut decrypted_data = Vec::new();
            let mut nonce = vec![0; 4];
            nonce.extend(b"PS-Msg05");
            chacha20_poly1305_aead::decrypt(
                // &encryption_key,
                &encryption_key,
                &nonce,
                &[],
                &encrypted_data,
                &auth_tag,
                &mut decrypted_data,
            )?;
            // TODO use :: ring::ChaCha20Poly1305MessageDecrypter

            let sub_tlv = tlv::decode(decrypted_data);
            let device_pairing_id = sub_tlv.get(&(Type::Identifier as u8)).ok_or(tlv::Error::Unknown)?;
            let device_ltpk = sub_tlv.get(&(Type::PublicKey as u8)).ok_or(tlv::Error::Unknown)?;
            let device_signature = sub_tlv.get(&(Type::Signature as u8)).ok_or(tlv::Error::Unknown)?;

            let mut device_x = [0; 32];
            let salt = hkdf::Salt::new(hkdf::HKDF_SHA512, b"Pair-Setup-Controller-Sign-Salt");
            let payload = PayloadU8Len(device_x.len());
            let PayloadU8(device_x) = salt.extract(&shared_secret)
                        .expand(&[b"Pair-Setup-Controller-Sign-Info"], payload)
                        .unwrap()
                        .into();
            let mut device_info: Vec<u8> = Vec::new();
            // device_info.extend(&device_x);
            device_info.extend(&device_x);
            device_info.extend(device_pairing_id);
            device_info.extend(device_ltpk);
            if !ed25519::verify(&device_info, &device_ltpk, &device_signature) {
                warn!("M5: Failed");
                return Err(tlv::Error::Authentication);
            }

            let uuid_str = str::from_utf8(device_pairing_id)?;
            debug!("Pairing UUID : {:?}", uuid_str);            
            let pairing_uuid = Uuid::parse_str(uuid_str)?;
            let mut pairing_ltpk = [0; 32];
            pairing_ltpk[..32].clone_from_slice(&device_ltpk[..32]);

            if let Some(max_peers) = config.lock().expect("couldn't access config").max_peers {
                if database.lock().expect("couldn't access database").count_pairings()? + 1 > max_peers {
                    return Err(tlv::Error::MaxPeers);
                }
            }

            let pairing = Pairing::new(pairing_uuid, Permissions::Admin, pairing_ltpk);
            pairing.save_to(database)?;

            let mut accessory_x = [0; 32];
            // let salt = hmac::SigningKey::new(&digest::SHA512, b"Pair-Setup-Accessory-Sign-Salt");
            // hkdf::extract_and_expand(
            //     &salt,
            //     &shared_secret,
            //     b"Pair-Setup-Accessory-Sign-Info",
            //     &mut accessory_x,
            // );
            let salt = hkdf::Salt::new(hkdf::HKDF_SHA512, b"Pair-Setup-Accessory-Sign-Salt");
            let payload = PayloadU8Len(accessory_x.len());
            let PayloadU8(accessory_x) = salt.extract(&shared_secret)
                        .expand(&[b"Pair-Setup-Accessory-Sign-Info"], payload)
                        .unwrap()
                        .into();

            let accessory = Device::load_from(database)?;
            let mut accessory_info: Vec<u8> = Vec::new();
            accessory_info.extend(&accessory_x);
            accessory_info.extend(accessory.id.as_bytes());
            accessory_info.extend(&accessory.public_key);
            let accessory_signature = ed25519::signature(&accessory_info, &accessory.private_key);

            let mut sub_tlv: HashMap<u8, Vec<u8>> = HashMap::new();
            Value::Identifier(accessory.id).into_map(&mut sub_tlv);
            Value::PublicKey(accessory.public_key.to_vec()).into_map(&mut sub_tlv);
            Value::Signature(accessory_signature.to_vec()).into_map(&mut sub_tlv);
            let encoded_sub_tlv = tlv::encode(sub_tlv);

            let mut encrypted_data = Vec::new();
            let mut nonce = vec![0; 4];
            nonce.extend(b"PS-Msg06");
            let auth_tag =
                chacha20_poly1305_aead::encrypt(&encryption_key, &nonce, &[], &encoded_sub_tlv, &mut encrypted_data)?;
            encrypted_data.extend(&auth_tag);

            event_emitter
                .lock()
                .expect("couldn't access event_emitter")
                .emit(&Event::DevicePaired);

            debug!("M6: Sending SRP Exchange Response");
            Ok(vec![
                Value::State(StepNumber::ExchangeRes as u8),
                Value::EncryptedData(encrypted_data),
            ])
        } else {
            warn!("M5: SRP Exch Req Error");
            Err(tlv::Error::Unknown)
        }
    } else {
        warn!("M5: SRP Exch Req Error - No Session");
        Err(tlv::Error::Unknown)
    }
}

fn verify_client_proof<D: Digest>(
    b_pub: &[u8],
    a_pub: &[u8],
    a_proof: &[u8],
    salt: &[u8],
    key: &[u8],
    group: &SrpGroup,
) -> Result<Vec<u8>, tlv::Error> {
    let mut dhn = D::new();
    dhn.input(&group.n.to_bytes_be());
    let hn = BigUint::from_bytes_be(&dhn.result());

    let mut dhg = D::new();
    dhg.input(&group.g.to_bytes_be());
    let hg = BigUint::from_bytes_be(&dhg.result());

    let hng = hn.bitxor(hg);

    let mut dhi = D::new();
    dhi.input(b"Pair-Setup");
    let hi = dhi.result();

    let mut d = D::new();
    // M = H(H(N) xor H(g), H(I), s, A, B, K)
    d.input(&hng.to_bytes_be());
    d.input(&hi);
    d.input(salt);
    d.input(a_pub);
    d.input(b_pub);
    d.input(key);

    if a_proof == d.result().as_slice() {
        // H(A, M, K)
        let mut d = D::new();
        d.input(a_pub);
        d.input(a_proof);
        d.input(key);
        Ok(d.result().as_slice().to_vec())
    } else {
        Err(tlv::Error::Authentication)
    }
}

/// Based off of whats in RustLS
/// An arbitrary, unknown-content, u8-length-prefixed payload
#[derive(Debug, Clone, PartialEq)]
pub struct PayloadU8(pub Vec<u8>);

impl PayloadU8 {
    pub fn new(bytes: Vec<u8>) -> PayloadU8 {
        PayloadU8(bytes)
    }

    pub fn empty() -> PayloadU8 {
        PayloadU8(Vec::new())
    }

    pub fn into_inner(self) -> Vec<u8> { self.0 }
}

pub(crate) struct PayloadU8Len(pub(crate) usize);
impl ring::hkdf::KeyType for PayloadU8Len {
    fn len(&self) -> usize { self.0 }
}

impl From<hkdf::Okm<'_, PayloadU8Len>> for PayloadU8 {
    fn from(okm: hkdf::Okm<PayloadU8Len>) -> Self {
        let mut r = vec![0u8;okm.len().0];
        okm.fill(&mut r[..]).unwrap();
        PayloadU8::new(r)
    }
}