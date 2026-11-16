//! DAVE, Discord's end-to-end encryption, for the stream connection: an MLS group of the
//! streamer and its viewers, driven by the stream gateway. One method per gateway opcode;
//! each returns the messages to send back.
//!
//! The session is davey 0.2.0 (`davey02`): its sender starts using a new key when the
//! gateway executes the transition, as the protocol says, so the viewers already watching
//! keep decrypting while somebody joins. (Voice, in `gateway::voice`, is on davey 0.1.4.)
//!
//! Rules this module enforces:
//! - while DAVE is in force and there is no key (nobody else in the group, or between two
//!   groups) nothing can be encrypted, and the caller sends nothing;
//! - a commit or welcome that cannot be processed is answered with opcode 31 and a new key
//!   package, at most three times in thirty seconds;
//! - the MLS group id of a stream is not documented: it is taken from the gateway's own
//!   messages when they show it, and guessed (`rtc_server_id - 1` first) until then.

use std::collections::{HashMap, VecDeque};
use std::num::NonZeroU16;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::time::{Duration, Instant};

use davey02::errors::{ProcessProposalsError, ProcessWelcomeError, SetExternalSenderError};
use davey02::{Codec, DaveSession, MediaType, ProposalsOperationType};
use log::{error, info, warn};
use serde_json::Value;

pub const OP_PREPARE_TRANSITION: u8 = 21;
pub const OP_EXECUTE_TRANSITION: u8 = 22;
pub const OP_READY_FOR_TRANSITION: u8 = 23;
pub const OP_PREPARE_EPOCH: u8 = 24;
pub const OP_EXTERNAL_SENDER: u8 = 25;
pub const OP_KEY_PACKAGE: u8 = 26;
pub const OP_PROPOSALS: u8 = 27;
pub const OP_COMMIT_WELCOME: u8 = 28;
pub const OP_ANNOUNCE_COMMIT: u8 = 29;
pub const OP_WELCOME: u8 = 30;
pub const OP_INVALID_COMMIT_WELCOME: u8 = 31;

/// Recoveries (opcode 31 and a new key package) allowed in `RECOVERY_WINDOW`.
const MAX_RECOVERIES: usize = 3;
const RECOVERY_WINDOW: Duration = Duration::from_secs(30);
/// How long the gateway may take to answer a commit (opcode 28) with an announcement (29)
/// or a welcome (30) before the log says that nothing came.
const COMMIT_ANSWER_WITHIN: Duration = Duration::from_secs(5);

/// A message for the stream gateway.
#[derive(Debug, Clone, PartialEq)]
pub enum Out {
    /// A binary frame: the opcode byte, then its payload.
    Binary(Vec<u8>),
    /// A JSON message, sent as text.
    Json(Value),
}

/// What may be sent now.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Sending {
    /// DAVE is not in force (protocol version 0): frames go out as they are.
    Plain,
    /// DAVE is in force and the group has a key for us.
    Encrypted,
    /// DAVE is in force and there is no key: nothing is sent.
    Nothing,
}

/// Which MLS group id to try first (`LITECORD_GOLIVE_GROUP`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum GroupChoice {
    /// `rtc_server_id - 1`, then `rtc_channel_id`, then `rtc_server_id`.
    #[default]
    Auto,
    Channel,
    Server,
    Id(u64),
}

impl GroupChoice {
    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "auto" => Some(Self::Auto),
            "channel" => Some(Self::Channel),
            "server" => Some(Self::Server),
            other => other.parse().ok().map(Self::Id),
        }
    }
}

/// The group ids to try, in order, each with the name the log gives it.
pub fn group_candidates(choice: GroupChoice, rtc_server_id: u64, rtc_channel_id: u64) -> Vec<(u64, &'static str)> {
    let known = [(rtc_server_id.wrapping_sub(1), "rtc_server_id - 1"), (rtc_channel_id, "rtc_channel_id"), (rtc_server_id, "rtc_server_id")];
    let mut out = Vec::with_capacity(4);
    match choice {
        GroupChoice::Auto => {}
        GroupChoice::Channel => out.push(known[1]),
        GroupChoice::Server => out.push(known[2]),
        GroupChoice::Id(id) => out.push((id, "LITECORD_GOLIVE_GROUP")),
    }
    for candidate in known {
        if !out.iter().any(|c: &(u64, &str)| c.0 == candidate.0) {
            out.push(candidate);
        }
    }
    out
}

/// The group id an MLS public message names in the clear: version (1), wire format (1,
/// public message), then the id as an 8-byte vector.
pub fn group_id_of_message(message: &[u8]) -> Option<u64> {
    if message.len() < 13 || message[..5] != [0, 1, 0, 1, 8] {
        return None;
    }
    Some(u64::from_be_bytes(message[5..13].try_into().ok()?))
}

/// The group id named by the first message of an "append" proposals payload (opcode 27,
/// after the operation byte): a length-prefixed vector of MLS messages.
pub fn group_id_of_proposals(proposals: &[u8]) -> Option<u64> {
    let skip = match proposals.first()? >> 6 {
        0 => 1,
        1 => 2,
        2 => 4,
        _ => return None,
    };
    group_id_of_message(proposals.get(skip..)?)
}

/// Runs a davey call fed by what the network sent; a panic in it comes back as an error.
fn guarded<T>(call: impl FnOnce() -> T) -> Result<T, String> {
    catch_unwind(AssertUnwindSafe(call)).map_err(|panic| {
        panic.downcast_ref::<&str>().map(|s| s.to_string()).or_else(|| panic.downcast_ref::<String>().cloned()).unwrap_or_else(|| "panic".to_string())
    })
}

pub struct Dave {
    user_id: u64,
    candidates: Vec<(u64, &'static str)>,
    /// Which of `candidates` is in use.
    candidate: usize,
    /// The group id read from one of the gateway's messages; used from then on.
    learned: Option<u64>,
    session: Option<DaveSession>,
    /// The protocol version the session was made for (opcodes 4 and 24); 0: none.
    session_version: u16,
    /// The protocol version in force for media; 0 means frames are not end-to-end encrypted.
    active_version: u16,
    external_sender: Option<Vec<u8>>,
    /// Transitions prepared and not yet executed: id -> protocol version.
    pending: HashMap<u16, u16>,
    /// The commit sent last (opcode 28), to tell whether the announced one is ours.
    own_commit: Option<Vec<u8>>,
    /// When that commit was sent, until the gateway answers it or the log has said it did not.
    commit_sent_at: Option<Instant>,
    /// Goes up whenever the key frames are encrypted with may have changed.
    key_generation: u64,
    recoveries: VecDeque<Instant>,
    /// Too many failures: nothing more is done until a new group or a new connection.
    stalled: bool,
    /// `LITECORD_GOLIVE_REQUIRE_DAVE=1`: never send without DAVE, whatever is negotiated.
    require_dave: bool,
}

impl Dave {
    pub fn new(user_id: u64, candidates: Vec<(u64, &'static str)>, require_dave: bool) -> Self {
        Self {
            user_id,
            candidates,
            candidate: 0,
            learned: None,
            session: None,
            session_version: 0,
            // Until the server says otherwise, assume DAVE: nothing goes out in the clear.
            active_version: 1,
            external_sender: None,
            pending: HashMap::new(),
            own_commit: None,
            commit_sent_at: None,
            key_generation: 0,
            recoveries: VecDeque::new(),
            stalled: false,
            require_dave,
        }
    }

    /// A new connection is being made (Identify): what the old one negotiated is void. The
    /// group id learned from the gateway is kept, it belongs to the stream.
    pub fn forget_connection(&mut self) {
        self.session = None;
        self.session_version = 0;
        self.active_version = 1;
        self.external_sender = None;
        self.pending.clear();
        self.own_commit = None;
        self.commit_sent_at = None;
        self.key_generation += 1;
        self.recoveries.clear();
        self.stalled = false;
    }

    /// True once when a commit sent with opcode 28 has gone unanswered for five seconds:
    /// neither announced (29) nor overtaken by a welcome (30). For the log; nothing is done
    /// about it (the gateway proposes again when it wants another commit).
    pub fn commit_unanswered(&mut self, now: Instant) -> bool {
        match self.commit_sent_at {
            Some(sent) if now.saturating_duration_since(sent) >= COMMIT_ANSWER_WITHIN => {
                self.commit_sent_at = None;
                true
            }
            _ => false,
        }
    }

    /// The group ids to try for the stream as the main gateway announced it. Another list
    /// than before means another stream instance: what was learned about the old one is void.
    pub fn set_group_candidates(&mut self, candidates: Vec<(u64, &'static str)>) {
        if self.candidates != candidates {
            self.candidates = candidates;
            self.candidate = 0;
            self.learned = None;
        }
    }

    pub fn group_id(&self) -> u64 {
        self.learned.unwrap_or_else(|| self.candidates.get(self.candidate).map_or(0, |c| c.0))
    }

    fn group_label(&self) -> &'static str {
        if self.learned.is_some() {
            "read from the gateway"
        } else {
            self.candidates.get(self.candidate).map_or("none", |c| c.1)
        }
    }

    pub fn sending(&self) -> Sending {
        if self.active_version == 0 {
            return if self.require_dave { Sending::Nothing } else { Sending::Plain };
        }
        if !self.stalled && self.session.as_ref().is_some_and(|s| s.can_encrypt()) {
            Sending::Encrypted
        } else {
            Sending::Nothing
        }
    }

    /// Whether media is end-to-end encrypted when it flows.
    pub fn end_to_end(&self) -> bool {
        self.active_version != 0
    }

    pub fn key_generation(&self) -> u64 {
        self.key_generation
    }

    pub fn stalled(&self) -> bool {
        self.stalled
    }

    pub fn epoch(&self) -> Option<u64> {
        self.session.as_ref().and_then(|s| s.epoch()).map(|e| e.as_u64())
    }

    /// The members of the group, ourselves included.
    pub fn members(&self) -> Vec<u64> {
        self.session.as_ref().and_then(|s| s.get_user_ids()).unwrap_or_default()
    }

    /// The 30-digit code official clients show for the call ("voice privacy code").
    pub fn privacy_code(&self) -> Option<String> {
        self.session.as_ref().and_then(|s| s.voice_privacy_code()).map(str::to_string)
    }

    /// Opcode 4, Session Description: the protocol version of this connection.
    pub fn on_session_description(&mut self, version: u16) -> Vec<Out> {
        self.session_version = version;
        self.active_version = version;
        self.pending.clear();
        self.own_commit = None;
        self.stalled = false;
        self.key_generation += 1;
        if version == 0 {
            self.session = None;
            if self.require_dave {
                warn!("[GO LIVE] DAVE: the server negotiated protocol version 0 (no end-to-end encryption) and LITECORD_GOLIVE_REQUIRE_DAVE=1 is set: nothing will be sent");
            } else {
                warn!("[GO LIVE] DAVE: the server negotiated protocol version 0: this stream is not end-to-end encrypted (only encrypted on the way to Discord's server)");
            }
            return Vec::new();
        }
        self.session = None;
        self.open_session("session created")
    }

    /// Opcode 25: the gateway's credential, with which it signs proposals.
    pub fn on_external_sender(&mut self, package: &[u8]) -> Vec<Out> {
        self.external_sender = Some(package.to_vec());
        let Some(session) = self.session.as_mut() else {
            info!("[GO LIVE] DAVE: external sender received ({} bytes), kept for the session to come", package.len());
            return Vec::new();
        };
        match guarded(|| session.set_external_sender(package)) {
            Ok(Ok(())) => info!("[GO LIVE] DAVE: external sender received ({} bytes) and set", package.len()),
            Ok(Err(SetExternalSenderError::AlreadyInGroup)) => info!("[GO LIVE] DAVE: external sender received while a group is established: ignored"),
            Ok(Err(e)) => warn!("[GO LIVE] DAVE: the external sender package was not accepted: {e}"),
            Err(panic) => {
                error!("[GO LIVE] DAVE: the external sender package crashed the MLS library ({panic}); starting the session over");
                self.session = None;
                self.external_sender = None;
                return self.open_session("session created again");
            }
        }
        Vec::new()
    }

    /// Opcode 27: `[operation][proposals]`. `recognised`: the users allowed into the group
    /// (`None`: no check).
    pub fn on_proposals(&mut self, payload: &[u8], recognised: Option<&[u64]>) -> Vec<Out> {
        let Some((&operation, proposals)) = payload.split_first() else {
            warn!("[GO LIVE] DAVE: empty proposals message (op 27): ignored");
            return Vec::new();
        };
        let mut out = Vec::new();
        let (kind, name) = if operation == 0 { (ProposalsOperationType::APPEND, "append") } else { (ProposalsOperationType::REVOKE, "revoke") };
        let mut id_note = "no group id readable in message";
        if operation == 0 {
            if let Some(named) = group_id_of_proposals(proposals) {
                if named == self.group_id() {
                    id_note = "group id in message matches";
                } else {
                    warn!(
                        "[GO LIVE] DAVE: the gateway names group {named}, we assumed {} ({}); starting over with the gateway's id",
                        self.group_id(),
                        self.group_label()
                    );
                    self.learned = Some(named);
                    self.session = None;
                    out.extend(self.open_session("session created again"));
                    id_note = "group id taken from message";
                }
            }
        }
        let Some(session) = self.session.as_mut() else {
            warn!("[GO LIVE] DAVE: proposals ({name}) with no session: ignored");
            return out;
        };
        match guarded(|| session.process_proposals(kind, proposals, recognised)) {
            Ok(Ok(Some(commit_welcome))) => {
                let mut message = Vec::with_capacity(1 + commit_welcome.commit.len() + commit_welcome.welcome.as_ref().map_or(0, Vec::len));
                message.push(OP_COMMIT_WELCOME);
                message.extend_from_slice(&commit_welcome.commit);
                // The welcome follows the commit directly, with nothing in between.
                if let Some(welcome) = &commit_welcome.welcome {
                    message.extend_from_slice(welcome);
                }
                info!(
                    "[GO LIVE] DAVE: proposals ({name}, {id_note}) -> commit {} bytes, welcome {} bytes sent (op 28)",
                    commit_welcome.commit.len(),
                    commit_welcome.welcome.as_ref().map_or(0, Vec::len)
                );
                self.own_commit = Some(commit_welcome.commit);
                self.commit_sent_at = Some(Instant::now());
                out.push(Out::Binary(message));
            }
            Ok(Ok(None)) => info!("[GO LIVE] DAVE: proposals ({name}, {id_note}): nothing left to commit"),
            Ok(Err(ProcessProposalsError::UnexpectedUser(user))) => {
                warn!("[GO LIVE] DAVE: proposal to add user {user}, who is neither in the voice channel nor announced by the stream server: refused (LITECORD_GOLIVE_DAVE_CHECK=0 turns this check off)");
            }
            Ok(Err(e)) => warn!("[GO LIVE] DAVE: proposals ({name}, {id_note}) ignored: {e}"),
            Err(panic) => {
                error!("[GO LIVE] DAVE: proposals crashed the MLS library ({panic})");
                out.extend(self.recover(0, true));
            }
        }
        out
    }

    /// Opcode 29: `[transition id][commit]`, the commit the gateway chose for the group.
    pub fn on_announce_commit(&mut self, transition: u16, commit: &[u8]) -> Vec<Out> {
        let ours = self.own_commit.as_deref() == Some(commit);
        self.commit_sent_at = None;
        if let Some(named) = group_id_of_message(commit) {
            if named != self.group_id() {
                warn!("[GO LIVE] DAVE: the announced commit names group {named}, ours is {} ({})", self.group_id(), self.group_label());
                self.learned = Some(named);
                return self.recover(transition, true);
            }
        }
        let version = self.session_version;
        let Some(session) = self.session.as_mut() else {
            warn!("[GO LIVE] DAVE: commit announced (op 29, transition {transition}) with no session: ignored");
            return Vec::new();
        };
        match guarded(|| session.process_commit(commit).map(|merged| merged.then(|| session.prepare_transition(transition, version)))) {
            Ok(Ok(Some(Ok(())))) => {
                self.own_commit = None;
                info!("[GO LIVE] DAVE: commit announced (op 29, transition {transition}), ours: {ours}");
                self.prepared(transition, version)
            }
            Ok(Ok(None)) => {
                info!("[GO LIVE] DAVE: commit announced (op 29, transition {transition}) is not for a group of ours: ignored");
                Vec::new()
            }
            Ok(Ok(Some(Err(e)))) => {
                warn!("[GO LIVE] DAVE: the commit of transition {transition} was merged but its keys could not be derived: {e}");
                self.recover(transition, false)
            }
            Ok(Err(e)) => {
                warn!("[GO LIVE] DAVE: the announced commit (transition {transition}, ours: {ours}) could not be processed: {e}");
                self.recover(transition, false)
            }
            Err(panic) => {
                error!("[GO LIVE] DAVE: the announced commit crashed the MLS library ({panic})");
                self.recover(transition, true)
            }
        }
    }

    /// Opcode 30: `[transition id][welcome]`, another member's commit added us.
    pub fn on_welcome(&mut self, transition: u16, welcome: &[u8], recognised: Option<&[u64]>) -> Vec<Out> {
        self.commit_sent_at = None;
        let version = self.session_version;
        let Some(session) = self.session.as_mut() else {
            warn!("[GO LIVE] DAVE: welcome (op 30, transition {transition}) with no session: ignored");
            return Vec::new();
        };
        match guarded(|| session.process_welcome(welcome, recognised).map(|()| session.prepare_transition(transition, version))) {
            Ok(Ok(Ok(()))) => {
                self.own_commit = None;
                info!("[GO LIVE] DAVE: welcomed (op 30, transition {transition})");
                self.prepared(transition, version)
            }
            Ok(Ok(Err(e))) => {
                warn!("[GO LIVE] DAVE: welcomed (transition {transition}) but the keys could not be derived: {e}");
                self.recover(transition, false)
            }
            Ok(Err(ProcessWelcomeError::AlreadyInGroup)) => {
                info!("[GO LIVE] DAVE: welcome (op 30, transition {transition}) while already in the group: ignored");
                Vec::new()
            }
            Ok(Err(ProcessWelcomeError::UnexpectedGroupId)) => {
                // A welcome does not show its group id. Try the next guess.
                let before = (self.group_id(), self.group_label());
                if self.learned.take().is_none() {
                    self.candidate = (self.candidate + 1) % self.candidates.len().max(1);
                }
                warn!(
                    "[GO LIVE] DAVE: the welcome (transition {transition}) is for another group than {} ({}); trying {} ({}) next",
                    before.0,
                    before.1,
                    self.group_id(),
                    self.group_label()
                );
                self.recover(transition, true)
            }
            Ok(Err(ProcessWelcomeError::UnrecognizedUser(user))) => {
                warn!("[GO LIVE] DAVE: the welcome (transition {transition}) is for a group with user {user}, who is neither in the voice channel nor announced by the stream server: refused");
                self.recover(transition, false)
            }
            Ok(Err(e)) => {
                warn!("[GO LIVE] DAVE: the welcome (transition {transition}) could not be processed: {e}");
                self.recover(transition, false)
            }
            Err(panic) => {
                error!("[GO LIVE] DAVE: the welcome crashed the MLS library ({panic})");
                self.recover(transition, true)
            }
        }
    }

    /// Opcode 21: the gateway prepares a change of protocol version (0: a downgrade to no
    /// end-to-end encryption), or, with transition 0, has a new group take effect at once.
    pub fn on_prepare_transition(&mut self, transition: u16, version: u16) -> Vec<Out> {
        if let Some(session) = self.session.as_mut() {
            match guarded(|| session.prepare_transition(transition, version)) {
                Ok(Ok(())) => {}
                Ok(Err(e)) => warn!("[GO LIVE] DAVE: prepare transition {transition} (protocol version {version}): {e}"),
                Err(panic) => {
                    error!("[GO LIVE] DAVE: prepare transition crashed the MLS library ({panic})");
                    return self.recover(transition, true);
                }
            }
        }
        info!("[GO LIVE] DAVE: transition {transition} to protocol version {version} prepared (op 21)");
        self.prepared(transition, version)
    }

    /// Opcode 22: the transition takes effect; from here frames use its key.
    pub fn on_execute_transition(&mut self, transition: u16) -> Vec<Out> {
        let Some(version) = self.pending.remove(&transition) else {
            info!("[GO LIVE] DAVE: execute for transition {transition} (op 22), which was never prepared: ignored");
            return Vec::new();
        };
        if let Some(session) = self.session.as_mut() {
            match guarded(|| session.execute_transition(transition)) {
                Ok(Ok(_)) => {}
                Ok(Err(e)) => warn!("[GO LIVE] DAVE: transition {transition} could not be executed: {e}"),
                Err(panic) => {
                    error!("[GO LIVE] DAVE: executing a transition crashed the MLS library ({panic})");
                    return self.recover(transition, true);
                }
            }
        }
        self.took_effect(transition, version);
        Vec::new()
    }

    /// Opcode 24: the gateway starts a new group (epoch 1): a new key package is due. The
    /// old key stays in use until the transition to the new group.
    pub fn on_prepare_epoch(&mut self, epoch: u64, version: u16) -> Vec<Out> {
        if epoch != 1 {
            info!("[GO LIVE] DAVE: prepare epoch {epoch} (op 24): nothing to do");
            return Vec::new();
        }
        if version == 0 {
            warn!("[GO LIVE] DAVE: prepare epoch with protocol version 0 (op 24): ignored");
            return Vec::new();
        }
        self.session_version = version;
        self.stalled = false;
        self.own_commit = None;
        self.commit_sent_at = None;
        self.restart("new group (op 24)")
    }

    /// A frame of H.264 (Annex B, 4-byte start codes), end-to-end encrypted; unchanged when
    /// DAVE is not in force. `None`: nothing may be sent, or the frame could not be
    /// encrypted (ten ciphertexts in a row held a start code).
    pub fn encrypt_video(&mut self, frame: &[u8]) -> Option<Vec<u8>> {
        match self.sending() {
            Sending::Plain => Some(frame.to_vec()),
            Sending::Nothing => None,
            Sending::Encrypted => {
                let session = self.session.as_mut()?;
                guarded(|| session.encrypt(MediaType::VIDEO, Codec::H264, frame).map(|f| f.into_owned())).ok()?.ok()
            }
        }
    }

    /// One Opus packet, end-to-end encrypted (the silence packet goes through as it is).
    pub fn encrypt_opus(&mut self, packet: &[u8]) -> Option<Vec<u8>> {
        match self.sending() {
            Sending::Plain => Some(packet.to_vec()),
            Sending::Nothing => None,
            Sending::Encrypted => {
                let session = self.session.as_mut()?;
                guarded(|| session.encrypt_opus(packet).map(|f| f.into_owned())).ok()?.ok()
            }
        }
    }

    /// A transition was prepared: with id 0 it is in effect, otherwise the gateway is told
    /// we are ready and executes it later.
    fn prepared(&mut self, transition: u16, version: u16) -> Vec<Out> {
        if transition == 0 {
            self.took_effect(0, version);
            return Vec::new();
        }
        self.pending.insert(transition, version);
        info!("[GO LIVE] DAVE: ready for transition {transition} (op 23 sent); the new key is used once the gateway executes it (op 22)");
        vec![Out::Json(super::wire::ready_for_transition(transition))]
    }

    fn took_effect(&mut self, transition: u16, version: u16) {
        let before = self.active_version;
        self.active_version = version;
        self.key_generation += 1;
        if version == 0 {
            warn!("[GO LIVE] DAVE: transition {transition} executed: protocol version 0, the stream is no longer end-to-end encrypted");
        } else if self.session.as_ref().is_some_and(|s| s.can_encrypt()) {
            info!(
                "[GO LIVE] DAVE: transition {transition} executed, epoch {}, {} in the group, privacy code {}",
                self.epoch().unwrap_or(0),
                self.members().len(),
                self.privacy_code().unwrap_or_else(|| "none".into())
            );
        } else {
            info!("[GO LIVE] DAVE: transition {transition} executed: no group with another member, no key (nothing is sent until somebody watches)");
        }
        if before == 0 && version != 0 {
            info!("[GO LIVE] DAVE: the stream is end-to-end encrypted from here");
        }
    }

    /// A new session object, the stored external sender, and a key package to send.
    fn open_session(&mut self, what: &str) -> Vec<Out> {
        let Some(version) = NonZeroU16::new(self.session_version) else { return Vec::new() };
        let (user, group) = (self.user_id, self.group_id());
        let external_sender = self.external_sender.clone();
        let made = guarded(|| -> Result<(DaveSession, Vec<u8>, Option<String>), String> {
            let mut session = DaveSession::new(version, user, group, None).map_err(|e| e.to_string())?;
            // (A package the session refuses is not fatal: the gateway sends it again.)
            let refused = external_sender.as_ref().and_then(|package| session.set_external_sender(package).err()).map(|e| e.to_string());
            let key_package = session.create_key_package().map_err(|e| e.to_string())?;
            Ok((session, key_package, refused))
        });
        match made {
            Ok(Ok((session, key_package, refused))) => {
                if let Some(e) = refused {
                    warn!("[GO LIVE] DAVE: the external sender package kept from before was not accepted ({e}); waiting for a new one");
                    self.external_sender = None;
                }
                info!(
                    "[GO LIVE] DAVE: {what}, protocol version {version}, group id {group} ({}); key package sent ({} bytes)",
                    self.group_label(),
                    key_package.len()
                );
                self.session = Some(session);
                self.key_generation += 1;
                vec![key_package_message(key_package)]
            }
            Ok(Err(e)) | Err(e) => {
                error!("[GO LIVE] DAVE: no session could be made ({e}); nothing can be sent on this connection");
                self.session = None;
                self.stalled = true;
                Vec::new()
            }
        }
    }

    /// The session starts over in the same identity (its keys stay until the next
    /// transition), with a new key package.
    fn restart(&mut self, what: &str) -> Vec<Out> {
        let Some(version) = NonZeroU16::new(self.session_version) else { return Vec::new() };
        let (user, group) = (self.user_id, self.group_id());
        let Some(session) = self.session.as_mut() else { return self.open_session(what) };
        match guarded(|| session.reinit(version, user, group, None).map_err(|e| e.to_string()).and_then(|()| session.create_key_package().map_err(|e| e.to_string()))) {
            Ok(Ok(key_package)) => {
                info!("[GO LIVE] DAVE: {what}, group id {group} ({}); key package sent ({} bytes)", self.group_label(), key_package.len());
                self.pending.clear();
                vec![key_package_message(key_package)]
            }
            Ok(Err(e)) | Err(e) => {
                warn!("[GO LIVE] DAVE: the session could not start over ({e}); making a new one");
                self.session = None;
                self.pending.clear();
                self.open_session(what)
            }
        }
    }

    /// A commit or welcome failed: tell the gateway (opcode 31) and offer a new key
    /// package, unless that was done three times in the last thirty seconds already.
    /// `fresh`: the session object is not to be trusted any more (or is for another group).
    fn recover(&mut self, transition: u16, fresh: bool) -> Vec<Out> {
        let now = Instant::now();
        while self.recoveries.front().is_some_and(|t| now.duration_since(*t) > RECOVERY_WINDOW) {
            self.recoveries.pop_front();
        }
        if self.recoveries.len() >= MAX_RECOVERIES {
            if !self.stalled {
                error!(
                    "[GO LIVE] DAVE: {} failures in {} s; giving up until the gateway starts a new group (op 24) or the stream connection is made again. The stream stays connected and sends nothing.",
                    self.recoveries.len() + 1,
                    RECOVERY_WINDOW.as_secs()
                );
            }
            self.stalled = true;
            self.key_generation += 1;
            return Vec::new();
        }
        self.recoveries.push_back(now);
        self.own_commit = None;
        let mut out = vec![Out::Json(super::wire::invalid_commit_welcome(transition))];
        if fresh {
            self.session = None;
        }
        out.extend(self.restart("invalid commit or welcome reported (op 31), session started over"));
        self.key_generation += 1;
        out
    }
}

fn key_package_message(key_package: Vec<u8>) -> Out {
    let mut message = Vec::with_capacity(1 + key_package.len());
    message.push(OP_KEY_PACKAGE);
    message.extend_from_slice(&key_package);
    Out::Binary(message)
}
