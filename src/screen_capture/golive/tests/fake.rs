//! Stand-ins for the other side of a stream: the voice gateway's MLS half (an external
//! sender that proposes members, as Discord's does), a viewer (a DAVE session of its own),
//! libwebrtc's receiver, and, for the state machine, a scripted stream gateway and media
//! server on 127.0.0.1.

use std::collections::VecDeque;
use std::net::SocketAddr;
use std::num::NonZeroU16;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use futures_util::{SinkExt, StreamExt};
use openmls::prelude::{tls_codec::Serialize, *};
use openmls_basic_credential::SignatureKeyPair;
use openmls_rust_crypto::OpenMlsRustCrypto;
use serde_json::Value;
use tokio::net::{TcpListener, UdpSocket};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
use tokio_tungstenite::tungstenite::protocol::CloseFrame;
use tokio_tungstenite::tungstenite::Message;

use super::super::crypto::{self, Mode, Transport};
use super::super::depacketize::Depacketizer;
use super::super::h264::{find_nalus, normalize_sps, Sps, NAL_SPS, START_CODE};
use super::super::{rtcp, udp};
use super::h264::webrtc_leaves_it_alone;

/// The MLS half of the voice gateway: it holds the external sender's key and turns key
/// packages into add proposals for a group.
pub struct MlsGateway {
    provider: OpenMlsRustCrypto,
    signer: SignatureKeyPair,
    external_sender: ExternalSender,
}

impl MlsGateway {
    pub fn new() -> Self {
        let suite = Ciphersuite::MLS_128_DHKEMP256_AES128GCM_SHA256_P256;
        let signer = SignatureKeyPair::new(suite.signature_algorithm()).unwrap();
        let external_sender = ExternalSender::new(signer.public().into(), BasicCredential::new(vec![0, 1, 1, 0]).into());
        Self { provider: OpenMlsRustCrypto::default(), signer, external_sender }
    }

    /// The payload of opcode 25.
    pub fn external_sender(&self) -> Vec<u8> {
        self.external_sender.tls_serialize_detached().unwrap()
    }

    /// The payload of opcode 27 (operation byte 0, then the vector of proposals) that adds
    /// the owners of `key_packages` to `group` at `epoch`.
    pub fn propose_add(&self, group: u64, epoch: u64, key_packages: &[&[u8]]) -> Vec<u8> {
        let mut messages = Vec::new();
        for key_package in key_packages {
            let key_package = KeyPackageIn::tls_deserialize_exact_bytes(key_package).unwrap().validate(self.provider.crypto(), ProtocolVersion::Mls10).unwrap();
            let proposal =
                ExternalProposal::new_add::<OpenMlsRustCrypto>(key_package, GroupId::from_slice(&group.to_be_bytes()), GroupEpoch::from(epoch), &self.signer, SenderExtensionIndex::new(0)).unwrap();
            messages.extend_from_slice(&proposal.tls_serialize_detached().unwrap());
        }
        [vec![0u8], VLBytes::new(messages).tls_serialize_detached().unwrap()].concat()
    }

    /// The payload of opcode 27 that removes the member at `leaf`.
    pub fn propose_remove(&self, group: u64, epoch: u64, leaf: u32) -> Vec<u8> {
        let proposal =
            ExternalProposal::new_remove::<OpenMlsRustCrypto>(LeafNodeIndex::new(leaf), GroupId::from_slice(&group.to_be_bytes()), GroupEpoch::from(epoch), &self.signer, SenderExtensionIndex::new(0)).unwrap();
        [vec![0u8], VLBytes::new(proposal.tls_serialize_detached().unwrap()).tls_serialize_detached().unwrap()].concat()
    }
}

/// The key package in an opcode 26 message (opcode byte first), checked to be one.
pub fn key_package_of(message: &[u8]) -> Vec<u8> {
    assert_eq!(message[0], 26, "a key package message");
    KeyPackageIn::tls_deserialize_exact_bytes(&message[1..]).expect("a key package");
    message[1..].to_vec()
}

/// The commit and the welcome of an opcode 28 message (opcode byte first): the welcome,
/// when there is one, follows the commit directly.
pub fn split_commit_welcome(message: &[u8]) -> (Vec<u8>, Option<Vec<u8>>) {
    assert_eq!(message[0], 28, "a commit/welcome message");
    let body = &message[1..];
    let (commit, rest) = MlsMessageIn::tls_deserialize_bytes(body).expect("opcode 28 starts with an MLS message");
    assert!(matches!(commit.wire_format(), WireFormat::PublicMessage), "the commit is a public message");
    let commit_len = body.len() - rest.len();
    if rest.is_empty() {
        return (body.to_vec(), None);
    }
    Welcome::tls_deserialize_exact_bytes(rest).expect("what follows the commit is a welcome and nothing else");
    (body[..commit_len].to_vec(), Some(rest.to_vec()))
}

/// Somebody watching: a DAVE session of their own. `Old` is davey 0.1.4, the version that
/// talks to official clients in voice calls; it has no transitions, its keys change when it
/// processes a commit or welcome.
pub enum Viewer {
    New(davey02::DaveSession),
    Old(davey::DaveSession),
}

impl Viewer {
    /// A viewer waiting to be added to `group`, and its key package.
    pub fn new(old: bool, user: u64, group: u64, gateway: &MlsGateway) -> (Self, Vec<u8>) {
        let version = NonZeroU16::new(1).unwrap();
        if old {
            let mut session = davey::DaveSession::new(version, user, group, None).unwrap();
            session.set_external_sender(&gateway.external_sender()).unwrap();
            let key_package = session.create_key_package().unwrap();
            (Viewer::Old(session), key_package)
        } else {
            let mut session = davey02::DaveSession::new(version, user, group, None).unwrap();
            session.set_external_sender(&gateway.external_sender()).unwrap();
            let key_package = session.create_key_package().unwrap();
            (Viewer::New(session), key_package)
        }
    }

    /// Opcode 30 at the viewer.
    pub fn welcomed(&mut self, welcome: &[u8], transition: u16) {
        match self {
            Viewer::New(s) => {
                s.process_welcome(welcome, None).unwrap();
                s.prepare_transition(transition, 1).unwrap();
            }
            Viewer::Old(s) => s.process_welcome(welcome).unwrap(),
        }
    }

    /// Opcode 29 at the viewer.
    pub fn committed(&mut self, commit: &[u8], transition: u16) {
        match self {
            Viewer::New(s) => {
                assert!(s.process_commit(commit).unwrap());
                s.prepare_transition(transition, 1).unwrap();
            }
            Viewer::Old(s) => s.process_commit(commit).unwrap(),
        }
    }

    /// Opcode 22 at the viewer.
    pub fn execute(&mut self, transition: u16) {
        if let Viewer::New(s) = self {
            s.execute_transition(transition).unwrap();
        }
    }

    /// Opcode 27 at the viewer: its own commit and welcome for the proposals.
    pub fn proposals(&mut self, payload: &[u8]) -> (Vec<u8>, Option<Vec<u8>>) {
        match self {
            Viewer::New(s) => {
                let made = s.process_proposals(davey02::ProposalsOperationType::APPEND, &payload[1..], None).unwrap().expect("a commit");
                (made.commit, made.welcome)
            }
            Viewer::Old(s) => {
                let made = s.process_proposals(davey::ProposalsOperationType::APPEND, &payload[1..], None).unwrap().expect("a commit");
                (made.commit, made.welcome)
            }
        }
    }

    pub fn decrypt_video(&mut self, from: u64, frame: &[u8]) -> Result<Vec<u8>, String> {
        match self {
            Viewer::New(s) => s.decrypt(from, davey02::MediaType::VIDEO, frame).map_err(|e| format!("{e:?}")),
            Viewer::Old(s) => s.decrypt(from, davey::MediaType::VIDEO, frame).map_err(|e| format!("{e:?}")),
        }
    }

    pub fn decrypt_audio(&mut self, from: u64, packet: &[u8]) -> Result<Vec<u8>, String> {
        match self {
            Viewer::New(s) => s.decrypt(from, davey02::MediaType::AUDIO, packet).map_err(|e| format!("{e:?}")),
            Viewer::Old(s) => s.decrypt(from, davey::MediaType::AUDIO, packet).map_err(|e| format!("{e:?}")),
        }
    }

    pub fn leaf(&self) -> u32 {
        match self {
            Viewer::New(s) => s.own_leaf_index().unwrap().u32(),
            Viewer::Old(s) => s.own_leaf_index().unwrap().u32(),
        }
    }

    pub fn epoch(&self) -> u64 {
        match self {
            Viewer::New(s) => s.epoch().unwrap().as_u64(),
            Viewer::Old(s) => s.epoch().unwrap().as_u64(),
        }
    }
}

/// What libwebrtc's receiver does to a frame between the network and the DAVE decryptor:
/// every NAL unit behind a 4-byte start code, and an SPS it does not like rewritten.
/// Whether it likes one is read by `h264-reader`, not by the normaliser under test.
pub fn through_webrtc_receiver(frame: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(frame.len() + 8);
    for n in find_nalus(frame) {
        let nal = &frame[n.payload..n.payload + n.len];
        out.extend_from_slice(&START_CODE);
        if nal[0] & 0x1f == NAL_SPS && !webrtc_leaves_it_alone(nal) {
            match normalize_sps(nal) {
                Some(Sps::Rewritten(rewritten)) => out.extend_from_slice(&rewritten),
                other => panic!("an SPS libwebrtc would rewrite, and the rewriter says {other:?}"),
            }
        } else {
            out.extend_from_slice(nal);
        }
    }
    out
}
