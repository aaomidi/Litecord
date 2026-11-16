//! Which interface sound, if any, an event of the call in progress deserves.
//!
//! The places that learn about a call (the gateway's voice states, the stream signaling,
//! the viewer list of the user's own stream, the voice connection) only report what they
//! saw, in one line each. Everything that makes that a sound or not is here:
//!
//! * only a call this app is in counts: one it has, or had until a moment ago, a voice
//!   connection to. A call the user holds on another device, or no call at all (a test
//!   binary driving the stream code), is silent;
//! * only the voice channel the user is connected to counts, and never the user's own
//!   voice state (their own join, leave, mute and deafen sounds are played where they act);
//! * nothing for who is already there, or already streaming, when the user arrives, and
//!   nothing for a state that is merely told again (a gateway READY, the signaling room
//!   after a reconnect): a sound needs a change from something that was known;
//! * one event, one sound: someone who leaves while streaming, or while watching the
//!   user's stream, is a leave, and the same sound is not repeated within
//!   `SAME_SOUND_EVERY` (five people arriving together are one arrival). The player adds
//!   its own rule: a sound that had to wait behind another for more than a moment is
//!   dropped (`sound_effects::STALE_AFTER`);
//! * what is only concluded is not believed at once. A Litecord stream "ends" for this
//!   client on a missed announcement or a signaling reconnect as well as on a real stop,
//!   so its end is held for `PEER_END_HOLD` and forgotten if the stream is still there
//!   (or if its owner left the channel: that is a leave). A voice connection that drops
//!   is only "disconnected" if it is not back after `CONNECTION_BACK_WITHIN`;
//! * for a moment after the user left or was disconnected, the call has nothing more to
//!   say (their stream ending, their viewers going, the voice connection closing);
//! * being deafened changes nothing: these are not the voices of the call but news about
//!   it, and someone arriving is exactly what a deafened user cannot notice otherwise
//!   (the official client plays them too). "Off" silences all of them, as it does the rest.

use std::collections::{HashMap, HashSet};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use log::info;

use super::sound_effects::{play_ui_sound, UiSound};

/// Voice states that arrive this soon after the user entered a channel describe who is in
/// it, whatever they look like: no "joined" sound for them.
const SETTLE_AFTER_ENTERING: Duration = Duration::from_millis(1500);
/// A stream first heard of this soon after entering was already running (senders repeat
/// their announcement every 1.5 s, and answer a newcomer in the signaling room at once).
const STREAMS_SETTLE: Duration = Duration::from_secs(6);
/// The same sound is not played again sooner than this.
const SAME_SOUND_EVERY: Duration = Duration::from_millis(1200);
/// After the user's own leave or a disconnection, how long the call stays silent.
const QUIET_AFTER_LEAVING: Duration = Duration::from_millis(1500);
/// A connection that keeps dropping says so once in this long.
const DISCONNECTED_EVERY: Duration = Duration::from_secs(5);
/// A voice connection that dropped has this long to come back unnoticed (the voice
/// gateway retries after 1.5 s), and one that keeps failing says so once in
/// `CONNECTION_LOST_EVERY`.
const CONNECTION_BACK_WITHIN: Duration = Duration::from_secs(4);
const CONNECTION_LOST_EVERY: Duration = Duration::from_secs(30);
/// How long a Litecord stream that seems to have ended is given to show it has not: a
/// stream nobody watches is only known by its announcements (one every 1.5 s, timed out
/// after 2.5 s), so one lost packet ends it for half a second, and so does its owner's
/// signaling reconnecting. Also the time its owner's leave has to arrive and be the sound.
const PEER_END_HOLD: Duration = Duration::from_secs(3);
/// The start of one peer's stream is announced once in this long; a start that was not
/// announced has no announced end either.
const PEER_START_EVERY: Duration = Duration::from_secs(6);
/// After the user left, the voice connection has this long to close without being taken
/// for a new call.
const CONNECTION_CLOSES_WITHIN: Duration = Duration::from_secs(10);
/// A viewer who is gone this soon after someone left the channel is that someone: their
/// watch request runs out up to 4 s (`watch::LEASE`) after they stopped renewing it.
const VIEWER_WENT_WITH_LEAVER: Duration = Duration::from_secs(5);

/// What this client knows of one peer's Litecord stream.
#[derive(Clone, Copy, Default)]
struct PeerStream {
    live: bool,
    /// When it was concluded to have stopped, while that is not believed yet.
    ending: Option<Instant>,
    /// When its start was last announced with a sound.
    announced_at: Option<Instant>,
    /// Its last start came too soon after the one before and was not announced.
    unannounced: bool,
}

/// What a report about a peer's stream comes to.
#[derive(Debug, Clone, Copy, PartialEq)]
enum StreamNews {
    Nothing,
    Sound(UiSound),
    /// It seems to have ended at this instant: ask again (`peer_stream_ended`) after
    /// `PEER_END_HOLD`.
    EndedMaybe(Instant),
}

/// What is known about the call, to tell a change from a repetition.
struct Calls {
    /// This app is in a call: it had a voice connection, and the user has not left or
    /// been taken out since. (Not the same as being connected right now: the connection
    /// may be down for a moment. And not concluded from the join sound, which also rings
    /// for an incoming call.)
    joined: bool,
    /// The voice channel the user is in, and since when.
    channel: Option<(String, Instant)>,
    /// The server of that channel, once the user's own voice state told it (`Some(None)`:
    /// a direct call, which has none).
    guild: Option<Option<String>>,
    /// Who is in that channel: whoever the gateway had there when the user entered, and
    /// whoever came since. A voice state of one of them is not an arrival.
    members: HashSet<u64>,
    /// Discord's own "Go Live" flag (`self_stream` of a voice state) of everyone in voice
    /// anywhere, so that it is known for whoever is in a channel the user enters later.
    go_live: HashMap<u64, bool>,
    /// The Litecord streams announced in the user's channel.
    peer_streams: HashMap<u64, PeerStream>,
    /// When each sound was last let through.
    last_played: Vec<(UiSound, Instant)>,
    /// When the user last left a channel or was disconnected.
    left_at: Option<Instant>,
    /// When someone else last left the user's channel.
    someone_left_at: Option<Instant>,
    /// When the user's own stream last ended.
    own_stream_ended_at: Option<Instant>,
    /// When a voice connection that did not come back was last announced.
    connection_lost_at: Option<Instant>,
}

static CALLS: std::sync::OnceLock<Mutex<Calls>> = std::sync::OnceLock::new();

fn calls() -> std::sync::MutexGuard<'static, Calls> {
    CALLS.get_or_init(|| Mutex::new(Calls::new())).lock().unwrap_or_else(|e| e.into_inner())
}

impl Calls {
    fn new() -> Self {
        Self { joined: false, channel: None, guild: None, members: HashSet::new(), go_live: HashMap::new(), peer_streams: HashMap::new(), last_played: Vec::new(), left_at: None, someone_left_at: None, own_stream_ended_at: None, connection_lost_at: None }
    }

    /// The user is in `channel` now (`None`: in none). A new channel is a new picture:
    /// what was known about streams belonged to the old one, and `here` is who the
    /// gateway has in the new one.
    fn follow(&mut self, channel: Option<&str>, now: Instant, here: &[u64]) {
        if self.channel.as_ref().map(|(id, _)| id.as_str()) != channel {
            self.channel = channel.map(|id| (id.to_string(), now));
            self.guild = None;
            self.members = here.iter().copied().collect();
            self.peer_streams.clear();
        }
    }

    /// The gateway told a voice state without it being an event (READY and the like, the
    /// voice states a call is announced with). `channel`: where it puts the user, if the
    /// caller knows.
    fn seed(&mut self, uid: u64, channel: Option<&str>, go_live: bool) {
        self.go_live.insert(uid, go_live);
        if channel.is_some() && self.channel.as_ref().map(|(id, _)| id.as_str()) == channel {
            self.members.insert(uid);
        }
    }

    /// A VOICE_STATE_UPDATE: `uid` was in `previous` (as far as the gateway's store knew)
    /// and is in `current` now, told by the server `guild` (`None`: a direct call).
    /// `my_channel` is the channel the user is connected to (not looked at for the user's
    /// own state, which is what changes it), `here` who the store has in the channel that
    /// becomes the user's with this event.
    #[allow(clippy::too_many_arguments)]
    fn voice_state(&mut self, now: Instant, uid: u64, is_me: bool, my_channel: Option<&str>, previous: Option<&str>, current: Option<&str>, guild: Option<&str>, go_live: bool, here: &[u64]) -> Option<UiSound> {
        if is_me {
            // Entering a channel, or coming back to it after the connection was reset.
            // (A mute or deafen of the user arrives here too, and changes nothing.)
            self.follow(current, now, here);
            if current.is_some() {
                self.guild = Some(guild.map(str::to_string));
            }
            return None;
        }
        self.follow(my_channel, now, here);
        // Every server tells its own side of a move: "in no channel" from another server
        // than the call's is about a channel there, even if it arrives after the same
        // user's arrival here (the store, which keeps one channel per user, cannot tell).
        if current.is_none() && self.channel.is_some() && self.guild.as_ref().is_some_and(|mine| mine.as_deref() != guild) {
            return None;
        }
        let was_live = if current.is_some() { self.go_live.insert(uid, go_live) } else { self.go_live.remove(&uid) };
        let (mine, since) = self.channel.as_ref()?;
        let was_here = self.members.contains(&uid) || previous == Some(mine.as_str());
        let is_here = current == Some(mine.as_str());
        match (was_here, is_here) {
            (false, true) => {
                self.members.insert(uid);
                (now.duration_since(*since) >= SETTLE_AFTER_ENTERING).then_some(UiSound::UserJoined)
            }
            (true, false) => {
                // Their stream goes with them, without a sound of its own (not even the
                // end that was being held back: see `PEER_END_HOLD`).
                self.members.remove(&uid);
                self.peer_streams.remove(&uid);
                self.someone_left_at = Some(now);
                Some(UiSound::UserLeft)
            }
            (true, true) => {
                self.members.insert(uid);
                match (was_live, go_live) {
                    (Some(false), true) => Some(UiSound::StreamStarted),
                    (Some(true), false) => Some(UiSound::StreamEnded),
                    // Mute, deafen, video..., or a flag never seen before: learnt, not played.
                    _ => None,
                }
            }
            (false, false) => None,
        }
    }

    /// A Litecord peer said (signaling presence, UDP announce) that it streams, or this
    /// client concluded (stop, signaling leave, timeout) that it does not any more.
    /// `in_channel`: the gateway has it in the user's voice channel.
    fn peer_stream(&mut self, now: Instant, uid: u64, in_channel: bool, streaming: bool) -> StreamNews {
        let Some((_, since)) = self.channel.as_ref() else { return StreamNews::Nothing };
        let since = *since;
        // Anyone can enter a signaling room under any id: only who is in the call is kept.
        if !in_channel {
            return StreamNews::Nothing;
        }
        let Some(peer) = self.peer_streams.get_mut(&uid) else {
            if streaming {
                // The first word from this peer: it was streaming when the user arrived,
                // unless the user has been here long enough to have heard of it.
                let started = now.duration_since(since) >= STREAMS_SETTLE;
                self.peer_streams.insert(uid, PeerStream { live: true, announced_at: started.then_some(now), ..Default::default() });
                return if started { StreamNews::Sound(UiSound::StreamStarted) } else { StreamNews::Nothing };
            }
            return StreamNews::Nothing;
        };
        match (peer.live, streaming) {
            (true, true) => {
                // Still there: whatever said it had ended was wrong.
                peer.ending = None;
                StreamNews::Nothing
            }
            (true, false) if peer.ending.is_none() => {
                peer.ending = Some(now);
                StreamNews::EndedMaybe(now)
            }
            (false, true) => {
                peer.live = true;
                peer.unannounced = peer.announced_at.is_some_and(|at| now.duration_since(at) < PEER_START_EVERY);
                if peer.unannounced {
                    return StreamNews::Nothing;
                }
                peer.announced_at = Some(now);
                StreamNews::Sound(UiSound::StreamStarted)
            }
            _ => StreamNews::Nothing,
        }
    }

    /// `PEER_END_HOLD` after `peer_stream` answered `EndedMaybe(since)`: it has ended if
    /// nothing said otherwise meanwhile (the stream again, its owner leaving, the user
    /// changing channel).
    fn peer_stream_ended(&mut self, uid: u64, since: Instant) -> Option<UiSound> {
        let peer = self.peer_streams.get_mut(&uid).filter(|peer| peer.ending == Some(since))?;
        peer.live = false;
        peer.ending = None;
        (!peer.unannounced).then_some(UiSound::StreamEnded)
    }

    /// The user's own stream started or ended.
    fn own_stream(&mut self, now: Instant, started: bool) -> Option<UiSound> {
        self.own_stream_ended_at = (!started).then_some(now);
        Some(if started { UiSound::StreamStarted } else { UiSound::StreamEnded })
    }

    /// Someone started or stopped watching the user's stream.
    fn viewer(&mut self, now: Instant, watching: bool) -> Option<UiSound> {
        if watching {
            return Some(UiSound::ViewerJoined);
        }
        // Not a second sound for what already had one: the viewer left the channel, or
        // the stream itself just ended (browser viewers are counted out after it).
        let went_with_leaver = self.someone_left_at.is_some_and(|at| now.duration_since(at) < VIEWER_WENT_WITH_LEAVER);
        let stream_over = self.own_stream_ended_at.is_some_and(|at| now.duration_since(at) < QUIET_AFTER_LEAVING);
        (!went_with_leaver && !stream_over).then_some(UiSound::ViewerLeft)
    }

    /// The server took the user out of the call (removed, the call ended, another client
    /// took it over). The call is over for this app.
    fn disconnected(&mut self, now: Instant) -> Option<UiSound> {
        self.joined = false;
        (!self.disconnected_recently(now)).then_some(UiSound::Disconnected)
    }

    /// The voice connection dropped `CONNECTION_BACK_WITHIN` ago and is not back. The user
    /// is still in the channel as far as the server goes, and the app keeps trying.
    fn connection_lost(&mut self, now: Instant) -> Option<UiSound> {
        if !self.joined || self.connection_lost_at.is_some_and(|at| now.duration_since(at) < CONNECTION_LOST_EVERY) {
            return None;
        }
        self.connection_lost_at = Some(now);
        (!self.disconnected_recently(now)).then_some(UiSound::Disconnected)
    }

    fn disconnected_recently(&self, now: Instant) -> bool {
        self.last_played.iter().any(|(s, at)| *s == UiSound::Disconnected && now.duration_since(*at) < DISCONNECTED_EVERY)
    }

    /// The last word on a sound an event asked for: not while the call is being left, and
    /// not the same sound again right away.
    fn allow(&mut self, now: Instant, sound: UiSound) -> bool {
        if self.left_at.is_some_and(|at| now.duration_since(at) < QUIET_AFTER_LEAVING) {
            return false;
        }
        if self.last_played.iter().any(|(s, at)| *s == sound && now.duration_since(*at) < SAME_SOUND_EVERY) {
            return false;
        }
        self.last_played.retain(|(s, _)| *s != sound);
        self.last_played.push((sound, now));
        true
    }

    /// Whether the voice connection is up was looked at: a connection makes it a call of
    /// this app, unless it is the one of a call just left, still closing.
    fn see_connection(&mut self, now: Instant, connected: bool) {
        if connected && !self.left_at.is_some_and(|at| now.duration_since(at) < CONNECTION_CLOSES_WITHIN) {
            self.joined = true;
        }
    }

    /// `play_ui_sound` was asked for `sound` (by anyone).
    fn requested(&mut self, now: Instant, sound: UiSound) {
        match sound {
            UiSound::LeaveChannel => {
                self.left_at = Some(now);
                self.joined = false;
            }
            UiSound::Disconnected => self.left_at = Some(now),
            UiSound::JoinChannel => {
                // Whoever the gateway tells about in the next moment is who was there.
                self.left_at = None;
                if let Some((_, since)) = self.channel.as_mut() {
                    *since = now;
                }
            }
            _ => {}
        }
    }
}

/// Plays `sound` if the rules above let it through. `what` is for the log.
fn play(decide: impl FnOnce(&mut Calls, Instant) -> Option<UiSound>, what: impl FnOnce() -> String) {
    let now = Instant::now();
    let connected = crate::gateway::is_connected_to_voice();
    let sound = {
        let mut calls = calls();
        calls.see_connection(now, connected);
        // Asked before the event is taken in: being taken out of the call ends it.
        let in_call = calls.joined || connected;
        // Taken in even outside a call, to know what changed once there is one.
        decide(&mut calls, now).filter(|sound| in_call && calls.allow(now, *sound))
    };
    // Played with the lock released: `play_ui_sound` reports back through `note_requested`.
    if let Some(sound) = sound {
        info!("[CALL SOUNDS] {:?}: {}", sound, what());
        play_ui_sound(sound);
    }
}

/// Runs `then` after `wait`, off the caller's thread (the callers are network loops).
fn later(wait: Duration, then: impl FnOnce() + Send + 'static) {
    let _ = std::thread::Builder::new().name("call-sounds-timer".to_string()).spawn(move || {
        std::thread::sleep(wait);
        then();
    });
}

/// Who the gateway's voice-state store has in `channel`.
fn users_in(channel: &str) -> Vec<u64> {
    crate::gateway::get_guild_voice_states_store()
        .lock()
        .map(|states| states.iter().filter(|(_, id)| id.as_str() == channel).map(|(uid, _)| *uid).collect())
        .unwrap_or_default()
}

fn json_id(value: &serde_json::Value) -> Option<String> {
    value.as_str().map(str::to_string).or_else(|| value.as_u64().map(|n| n.to_string()))
}

/// A voice state the gateway listed without it being an event (READY, READY_SUPPLEMENTAL,
/// GUILD_CREATE): remembered so that a later change can be told from it.
pub fn seed_voice_state(uid: u64, go_live: bool) {
    calls().seed(uid, None, go_live);
}

/// The voice states a direct or group call is announced with (the `d` of CALL_CREATE and
/// CALL_UPDATE): who is in the call already. The gateway handler does not keep them, so
/// they are put in the voice-state store here; without that, the first mute of someone
/// who was in the call before the user answered would be taken for their arrival.
pub fn seed_call(call: &serde_json::Value) {
    let Some(states) = call["voice_states"].as_array() else { return };
    let me = crate::gateway::get_my_user_id();
    for state in states {
        let uid = json_id(&state["user_id"]).and_then(|id| id.parse::<u64>().ok()).unwrap_or(0);
        let Some(channel) = json_id(&state["channel_id"]).or_else(|| json_id(&call["channel_id"])) else { continue };
        if uid == 0 || uid == me || channel.is_empty() {
            continue;
        }
        // The two locks are never held together.
        if let Ok(mut store) = crate::gateway::get_guild_voice_states_store().lock() {
            store.insert(uid, channel.clone());
        }
        calls().seed(uid, Some(&channel), state["self_stream"].as_bool().unwrap_or(false));
    }
}

/// A VOICE_STATE_UPDATE of any user in any server: `uid` was in the voice channel
/// `previous` and is in `current` now (`None`: in none); `state` is the event's `d`
/// (Discord's own stream flag and the server are read from it). `my_channel` is the
/// channel the user is connected to.
pub fn on_voice_state(uid: u64, my_channel: Option<&str>, previous: Option<&str>, current: Option<&str>, state: &serde_json::Value) {
    let is_me = uid != 0 && uid == crate::gateway::get_my_user_id();
    let go_live = state["self_stream"].as_bool().unwrap_or(false);
    let guild = json_id(&state["guild_id"]);
    // If this event changes the channel the user is in: who the gateway has there. (The
    // two locks are never held together.)
    let becomes = if is_me { current } else { my_channel };
    let entering = calls().channel.as_ref().map(|(id, _)| id.as_str()) != becomes;
    let here = if entering { becomes.map(users_in).unwrap_or_default() } else { Vec::new() };
    play(
        |calls, now| calls.voice_state(now, uid, is_me, my_channel, previous, current, guild.as_deref(), go_live, &here),
        || format!("voice state of user {uid} ({previous:?} -> {current:?}, go live: {go_live})"),
    );
}

/// A Litecord peer's stream: `true` as often as it is announced to this client (signaling
/// presence, UDP announce), `false` when this client concludes that it ended
/// (`trigger_stream_stopped`: stop packet, presence, signaling leave, timeout). Only a
/// change in the user's channel is heard, and an end only once it has lasted.
pub fn on_peer_stream(uid: u64, streaming: bool) {
    if uid == 0 || uid == crate::gateway::get_my_user_id() {
        return;
    }
    // The two locks are never held together.
    let channel = calls().channel.as_ref().map(|(id, _)| id.clone());
    let in_channel = channel.is_some_and(|id| {
        crate::gateway::get_guild_voice_states_store().lock().is_ok_and(|states| states.get(&uid) == Some(&id))
    });
    let mut held = None;
    play(
        |calls, now| match calls.peer_stream(now, uid, in_channel, streaming) {
            StreamNews::Sound(sound) => Some(sound),
            StreamNews::EndedMaybe(since) => {
                held = Some(since);
                None
            }
            StreamNews::Nothing => None,
        },
        || format!("stream of peer {uid} started"),
    );
    if let Some(since) = held {
        later(PEER_END_HOLD, move || {
            play(|calls, _| calls.peer_stream_ended(uid, since), || format!("stream of peer {uid} ended"));
        });
    }
}

/// The user's own stream started or ended.
pub fn on_own_stream(started: bool) {
    play(
        |calls, now| calls.own_stream(now, started),
        || format!("own stream {}", if started { "started" } else { "ended" }),
    );
}

/// Someone started or stopped watching the user's stream (`who`: for the log).
pub fn on_viewer(watching: bool, who: &str) {
    play(
        |calls, now| calls.viewer(now, watching),
        || format!("{who} {} watching the stream", if watching { "started" } else { "stopped" }),
    );
}

/// The user was taken out of voice without asking: removed from the channel, the call
/// ended by the server, or the voice connection lost (`why`: for the log).
pub fn on_disconnected(why: &str) {
    play(|calls, now| calls.disconnected(now), || why.to_string());
}

/// The voice connection dropped and is being reconnected. Not news if it is back within
/// `CONNECTION_BACK_WITHIN`: the user never left the channel.
pub fn on_voice_connection_lost() {
    later(CONNECTION_BACK_WITHIN, || {
        if !crate::gateway::is_connected_to_voice() {
            play(|calls, now| calls.connection_lost(now), || "voice connection lost and not back yet".to_string());
        }
    });
}

/// `play_ui_sound` was asked for `sound`: the user's own leave (and a disconnection)
/// starts the silence in which the call's last events are not announced.
pub(super) fn note_requested(sound: UiSound) {
    let connected = crate::gateway::is_connected_to_voice();
    {
        let mut calls = calls();
        let now = Instant::now();
        calls.see_connection(now, connected);
        calls.requested(now, sound);
    }
    // A join is on its way (or a call is ringing): look whether it became a voice
    // connection, so that the call is known as this app's even if nothing else happens in
    // it before the connection drops or the user is taken out.
    if sound == UiSound::JoinChannel {
        let _ = std::thread::Builder::new().name("call-sounds-timer".to_string()).spawn(|| {
            for wait in [3, 5, 12] {
                std::thread::sleep(Duration::from_secs(wait));
                let connected = crate::gateway::is_connected_to_voice();
                calls().see_connection(Instant::now(), connected);
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ME: u64 = 1;
    const ANA: u64 = 2;
    const BO: u64 = 3;
    const ROOM: Option<&str> = Some("100");
    const OTHER: Option<&str> = Some("200");

    /// A voice state told by the server of ROOM, with nobody known to be in the channel
    /// (who was there is seeded by `in_room`).
    fn state(calls: &mut Calls, at: Instant, uid: u64, is_me: bool, mine: Option<&str>, previous: Option<&str>, current: Option<&str>, go_live: bool) -> Option<UiSound> {
        calls.voice_state(at, uid, is_me, mine, previous, current, Some("7"), go_live, &[])
    }

    fn secs(start: Instant, s: f64) -> Instant {
        start + Duration::from_secs_f64(s)
    }

    /// The user in ROOM since `start`, Ana already there (seeded, not streaming).
    fn in_room(start: Instant) -> Calls {
        let mut calls = Calls::new();
        calls.seed(ANA, None, false);
        // The app has already put the user in the channel when the gateway's echo arrives;
        // the store has Ana there.
        assert_eq!(calls.voice_state(start, ME, true, ROOM, ROOM, ROOM, Some("7"), false, &[ANA]), None);
        calls
    }

    #[test]
    fn call_sounds_for_others_joining_and_leaving_only_in_my_channel() {
        let t = Instant::now();
        let mut calls = in_room(t);
        // Bo enters, moves out to another channel, comes back, disconnects.
        assert_eq!(state(&mut calls, secs(t, 10.0), BO, false, ROOM, None, ROOM, false), Some(UiSound::UserJoined));
        assert_eq!(state(&mut calls, secs(t, 20.0), BO, false, ROOM, ROOM, OTHER, false), Some(UiSound::UserLeft));
        assert_eq!(state(&mut calls, secs(t, 30.0), BO, false, ROOM, OTHER, ROOM, false), Some(UiSound::UserJoined));
        assert_eq!(state(&mut calls, secs(t, 40.0), BO, false, ROOM, ROOM, None, false), Some(UiSound::UserLeft));
        // Elsewhere: another channel, another server.
        assert_eq!(state(&mut calls, secs(t, 50.0), BO, false, ROOM, None, OTHER, false), None);
        assert_eq!(state(&mut calls, secs(t, 51.0), BO, false, ROOM, OTHER, Some("300"), true), None);
        assert_eq!(state(&mut calls, secs(t, 52.0), BO, false, ROOM, Some("300"), None, false), None);
        // Ana mutes: a voice state of someone who stays.
        assert_eq!(state(&mut calls, secs(t, 60.0), ANA, false, ROOM, ROOM, ROOM, false), None);
        // The user's own states never sound here (mute, then leaving).
        assert_eq!(state(&mut calls, secs(t, 70.0), ME, true, ROOM, ROOM, ROOM, false), None);
        assert_eq!(state(&mut calls, secs(t, 80.0), ME, true, None, ROOM, None, false), None);
        // Not in a channel any more: nothing, whoever moves.
        assert_eq!(state(&mut calls, secs(t, 90.0), BO, false, None, None, ROOM, false), None);
        assert_eq!(state(&mut calls, secs(t, 91.0), ANA, false, None, ROOM, None, false), None);
    }

    #[test]
    fn call_sounds_stay_silent_for_who_was_there_and_for_replays() {
        let t = Instant::now();
        let mut calls = Calls::new();
        // The user joins; the channel's members are told right after, as updates the
        // store had not seen (a server whose voice states were not in READY).
        assert_eq!(state(&mut calls, t, ME, true, ROOM, ROOM, ROOM, false), None);
        assert_eq!(state(&mut calls, secs(t, 0.2), ANA, false, ROOM, None, ROOM, true), None);
        assert_eq!(state(&mut calls, secs(t, 0.3), BO, false, ROOM, None, ROOM, false), None);
        // Ana was streaming all along: her next state (a mute) is not a stream starting.
        assert_eq!(state(&mut calls, secs(t, 30.0), ANA, false, ROOM, ROOM, ROOM, true), None);
        // The connection is reset: the user is taken out and put back, and the members
        // are told again. Nobody arrived.
        assert_eq!(state(&mut calls, secs(t, 40.0), ME, true, ROOM, ROOM, None, false), None);
        assert_eq!(state(&mut calls, secs(t, 40.3), ME, true, ROOM, None, ROOM, false), None);
        assert_eq!(state(&mut calls, secs(t, 40.5), BO, false, ROOM, None, ROOM, false), None);
        // A gateway READY lists everyone again (seed) and the store already has them:
        // the next update of each is a state of someone who stays.
        calls.seed(ANA, None, true);
        calls.seed(BO, None, false);
        assert_eq!(state(&mut calls, secs(t, 60.0), ANA, false, ROOM, ROOM, ROOM, true), None);
        assert_eq!(state(&mut calls, secs(t, 61.0), BO, false, ROOM, ROOM, ROOM, false), None);
        // The channel is first learnt from someone else's event (the user's own echo was
        // lost): that event describes the channel too.
        let mut calls = Calls::new();
        assert_eq!(state(&mut calls, t, BO, false, ROOM, None, ROOM, false), None);
        assert_eq!(state(&mut calls, secs(t, 5.0), ANA, false, ROOM, None, ROOM, false), Some(UiSound::UserJoined));
    }

    #[test]
    fn call_sounds_for_discord_streams_need_a_known_change() {
        let t = Instant::now();
        let mut calls = in_room(t);
        // Ana (known not to stream) goes live, mutes while live, stops.
        assert_eq!(state(&mut calls, secs(t, 5.0), ANA, false, ROOM, ROOM, ROOM, true), Some(UiSound::StreamStarted));
        assert_eq!(state(&mut calls, secs(t, 6.0), ANA, false, ROOM, ROOM, ROOM, true), None);
        assert_eq!(state(&mut calls, secs(t, 9.0), ANA, false, ROOM, ROOM, ROOM, false), Some(UiSound::StreamEnded));
        // Bo arrives (join sound only), starts, and leaves while live: one sound, the leave.
        assert_eq!(state(&mut calls, secs(t, 10.0), BO, false, ROOM, None, ROOM, false), Some(UiSound::UserJoined));
        assert_eq!(state(&mut calls, secs(t, 12.0), BO, false, ROOM, ROOM, ROOM, true), Some(UiSound::StreamStarted));
        assert_eq!(state(&mut calls, secs(t, 14.0), BO, false, ROOM, ROOM, None, false), Some(UiSound::UserLeft));
        // A stream in another channel is not ours, but is remembered: when its owner comes
        // over still live and then stops, that is a stream ending here.
        assert_eq!(state(&mut calls, secs(t, 20.0), BO, false, ROOM, None, OTHER, false), None);
        assert_eq!(state(&mut calls, secs(t, 21.0), BO, false, ROOM, OTHER, OTHER, true), None);
        assert_eq!(state(&mut calls, secs(t, 22.0), BO, false, ROOM, OTHER, ROOM, true), Some(UiSound::UserJoined));
        assert_eq!(state(&mut calls, secs(t, 23.0), BO, false, ROOM, ROOM, ROOM, false), Some(UiSound::StreamEnded));
    }

    #[test]
    fn call_sounds_for_peer_streams_skip_what_was_already_running() {
        use StreamNews::{EndedMaybe, Nothing, Sound};
        let t = Instant::now();
        let mut calls = in_room(t);
        // On arrival: Ana is streaming. Only learnt.
        assert_eq!(calls.peer_stream(secs(t, 0.8), ANA, true, true), Nothing);
        // The same told again (UDP announce every 1.5 s, the signaling room after a reconnect).
        assert_eq!(calls.peer_stream(secs(t, 2.4), ANA, true, true), Nothing);
        assert_eq!(calls.peer_stream(secs(t, 30.0), ANA, true, true), Nothing);
        // Bo starts long after the user arrived.
        assert_eq!(calls.peer_stream(secs(t, 40.0), BO, true, true), Sound(UiSound::StreamStarted));
        // Ana stops: believed once it has lasted, however many sides conclude it (stop
        // packet, presence, timeout).
        assert_eq!(calls.peer_stream(secs(t, 50.0), ANA, true, false), EndedMaybe(secs(t, 50.0)));
        assert_eq!(calls.peer_stream(secs(t, 50.1), ANA, true, false), Nothing);
        assert_eq!(calls.peer_stream(secs(t, 52.5), ANA, true, false), Nothing);
        assert_eq!(calls.peer_stream_ended(ANA, secs(t, 50.0)), Some(UiSound::StreamEnded));
        assert_eq!(calls.peer_stream(secs(t, 53.5), ANA, true, false), Nothing);
        // Someone who is not in the user's voice channel (anyone can enter a signaling
        // room): nothing, and nothing kept.
        assert_eq!(calls.peer_stream(secs(t, 61.0), 10, false, false), Nothing);
        assert_eq!(calls.peer_stream(secs(t, 62.0), 10, false, true), Nothing);
        assert!(!calls.peer_streams.contains_key(&10));
        // A "not streaming" for a peer nothing is known of (a forged presence, repeated).
        assert_eq!(calls.peer_stream(secs(t, 63.0), 11, true, false), Nothing);
        assert_eq!(calls.peer_stream(secs(t, 63.5), 11, true, false), Nothing);
        // The user moves to another channel: what is streaming there is learnt afresh.
        assert_eq!(state(&mut calls, secs(t, 80.0), ME, true, ROOM, ROOM, OTHER, false), None);
        assert_eq!(calls.peer_stream(secs(t, 80.5), 9, true, true), Nothing);
        // Out of voice: nothing.
        assert_eq!(state(&mut calls, secs(t, 90.0), ME, true, OTHER, OTHER, None, false), None);
        assert_eq!(calls.peer_stream(secs(t, 91.0), 9, true, false), Nothing);
    }

    #[test]
    fn call_sounds_do_not_believe_a_stream_that_only_seems_to_end() {
        use StreamNews::{EndedMaybe, Nothing, Sound};
        let t = Instant::now();
        let mut calls = in_room(t);
        assert_eq!(calls.peer_stream(secs(t, 60.0), ANA, true, true), Sound(UiSound::StreamStarted));
        // Nobody watches it and one announcement is lost: timed out at 2.5 s, announced
        // again half a second later. Neither an end nor a start.
        assert_eq!(calls.peer_stream(secs(t, 62.5), ANA, true, false), EndedMaybe(secs(t, 62.5)));
        assert_eq!(calls.peer_stream(secs(t, 63.0), ANA, true, true), Nothing);
        assert_eq!(calls.peer_stream_ended(ANA, secs(t, 62.5)), None);
        // It flaps for half a minute (a signaling socket that keeps reconnecting, or
        // someone forging presences): not one sound.
        let mut heard = 0;
        for i in 0..40 {
            let at = secs(t, 64.0 + 0.7 * f64::from(i));
            match calls.peer_stream(at, ANA, true, i % 2 == 1) {
                Sound(_) => heard += 1,
                EndedMaybe(since) => assert_eq!(since, at),
                Nothing => {}
            }
        }
        assert_eq!(heard, 0);
        // A real end right after a start, and a restart at once: the end is told, the
        // second start is not (one per peer in `PEER_START_EVERY`), nor is its end.
        let mut calls = in_room(t);
        assert_eq!(calls.peer_stream(secs(t, 60.0), ANA, true, true), Sound(UiSound::StreamStarted));
        assert_eq!(calls.peer_stream(secs(t, 61.0), ANA, true, false), EndedMaybe(secs(t, 61.0)));
        assert_eq!(calls.peer_stream_ended(ANA, secs(t, 61.0)), Some(UiSound::StreamEnded));
        assert_eq!(calls.peer_stream(secs(t, 64.5), ANA, true, true), Nothing);
        assert_eq!(calls.peer_stream(secs(t, 65.0), ANA, true, false), EndedMaybe(secs(t, 65.0)));
        assert_eq!(calls.peer_stream_ended(ANA, secs(t, 65.0)), None);
        assert_eq!(calls.peer_stream(secs(t, 80.0), ANA, true, true), Sound(UiSound::StreamStarted));
        // Its owner stops the stream and leaves (Litecord sends the stop first): the end
        // is still being held when the leave arrives, and the leave is the one sound.
        assert_eq!(calls.peer_stream(secs(t, 90.0), ANA, true, false), EndedMaybe(secs(t, 90.0)));
        assert_eq!(state(&mut calls, secs(t, 90.2), ANA, false, ROOM, ROOM, None, false), Some(UiSound::UserLeft));
        assert_eq!(calls.peer_stream_ended(ANA, secs(t, 90.0)), None);
        // The other order: the leave first, then the stop of someone no longer here.
        assert_eq!(state(&mut calls, secs(t, 100.0), BO, false, ROOM, None, ROOM, false), Some(UiSound::UserJoined));
        assert_eq!(calls.peer_stream(secs(t, 110.0), BO, true, true), Sound(UiSound::StreamStarted));
        assert_eq!(state(&mut calls, secs(t, 120.0), BO, false, ROOM, ROOM, None, false), Some(UiSound::UserLeft));
        assert_eq!(calls.peer_stream(secs(t, 120.1), BO, false, false), Nothing);
    }

    #[test]
    fn call_sounds_know_who_was_in_the_call_and_which_server_speaks() {
        let t = Instant::now();
        // A direct call: Ana was in it before the user answered. CALL_CREATE listed her
        // (the store has her, so she is `here` when the user enters); her first voice
        // state a minute later, a mute or going live, is not an arrival.
        let mut calls = Calls::new();
        calls.seed(ANA, ROOM, false);
        assert_eq!(calls.voice_state(t, ME, true, ROOM, None, ROOM, None, false, &[ANA]), None);
        assert_eq!(calls.voice_state(secs(t, 60.0), ANA, false, ROOM, None, ROOM, None, false, &[]), None);
        assert_eq!(calls.voice_state(secs(t, 70.0), ANA, false, ROOM, ROOM, ROOM, None, true, &[]), Some(UiSound::StreamStarted));
        // The call's states told while the user is already in it (CALL_UPDATE): Bo is
        // there, not arriving.
        calls.seed(BO, ROOM, false);
        assert_eq!(calls.voice_state(secs(t, 80.0), BO, false, ROOM, None, ROOM, None, false, &[]), None);
        // A server call (server "7"). Bo comes over from a channel of server "8", and
        // that server's "left" arrives after the arrival here: not a leave. The store
        // has lost him by then (previous: none); his real leave is still heard.
        let mut calls = in_room(t);
        assert_eq!(calls.voice_state(secs(t, 10.0), BO, false, ROOM, OTHER, ROOM, Some("7"), false, &[]), Some(UiSound::UserJoined));
        assert_eq!(calls.voice_state(secs(t, 10.1), BO, false, ROOM, ROOM, None, Some("8"), false, &[]), None);
        assert_eq!(calls.voice_state(secs(t, 20.0), BO, false, ROOM, None, ROOM, Some("7"), false, &[]), None);
        assert_eq!(calls.voice_state(secs(t, 30.0), BO, false, ROOM, None, None, Some("7"), false, &[]), Some(UiSound::UserLeft));
    }

    #[test]
    fn call_sounds_need_a_call_of_this_app() {
        let t = Instant::now();
        let mut calls = Calls::new();
        // Nothing joined: a test binary starting streams, a call held on another device.
        assert!(!calls.joined);
        // The join sound alone is not a call (it also rings for an incoming one); the
        // voice connection is.
        calls.requested(t, UiSound::JoinChannel);
        calls.see_connection(secs(t, 3.0), false);
        assert!(!calls.joined);
        calls.see_connection(secs(t, 8.0), true);
        assert!(calls.joined);
        // Leaving ends it, and the connection that is still closing does not bring it back.
        calls.requested(secs(t, 20.0), UiSound::LeaveChannel);
        assert!(!calls.joined);
        calls.see_connection(secs(t, 20.5), true);
        assert!(!calls.joined);
        // The next call. A connection that drops does not end it: the app is still in
        // the call, and says so once if the connection does not come back.
        calls.requested(secs(t, 100.0), UiSound::JoinChannel);
        calls.see_connection(secs(t, 101.0), true);
        assert!(calls.joined);
        assert_eq!(calls.connection_lost(secs(t, 120.0)), Some(UiSound::Disconnected));
        assert!(calls.joined);
        assert_eq!(calls.connection_lost(secs(t, 126.0)), None);
        assert_eq!(calls.connection_lost(secs(t, 155.0)), Some(UiSound::Disconnected));
        // Being taken out by the server ends it.
        assert_eq!(calls.disconnected(secs(t, 190.0)), Some(UiSound::Disconnected));
        assert!(!calls.joined);
        // A connection that was being waited for is not news any more.
        assert_eq!(calls.connection_lost(secs(t, 230.0)), None);
    }

    #[test]
    fn call_sounds_do_not_repeat_and_stop_when_the_user_leaves() {
        let t = Instant::now();
        let mut calls = in_room(t);
        // Three people arrive within a second: one sound. A fourth later: another.
        assert!(calls.allow(secs(t, 10.0), UiSound::UserJoined));
        assert!(!calls.allow(secs(t, 10.3), UiSound::UserJoined));
        assert!(!calls.allow(secs(t, 10.9), UiSound::UserJoined));
        assert!(calls.allow(secs(t, 11.3), UiSound::UserJoined));
        // A different event in between is still told.
        assert!(calls.allow(secs(t, 11.4), UiSound::StreamStarted));
        // The user leaves: their stream ending and their viewers going make no sound, nor
        // does the voice connection closing; later events do again.
        calls.requested(secs(t, 20.0), UiSound::LeaveChannel);
        assert!(!calls.allow(secs(t, 20.1), UiSound::StreamEnded));
        assert!(!calls.allow(secs(t, 20.2), UiSound::ViewerLeft));
        assert!(!calls.allow(secs(t, 20.4), UiSound::Disconnected));
        assert!(calls.allow(secs(t, 22.0), UiSound::StreamEnded));
        // Joining ends that silence at once.
        calls.requested(secs(t, 30.0), UiSound::LeaveChannel);
        calls.requested(secs(t, 30.2), UiSound::JoinChannel);
        assert!(calls.allow(secs(t, 30.4), UiSound::ViewerJoined));
    }

    #[test]
    fn call_sounds_for_viewers_but_not_for_one_who_left_the_channel() {
        let t = Instant::now();
        let mut calls = in_room(t);
        assert_eq!(calls.viewer(secs(t, 10.0), true), Some(UiSound::ViewerJoined));
        assert_eq!(calls.viewer(secs(t, 20.0), false), Some(UiSound::ViewerLeft));
        // Ana watches, then leaves the channel: her request runs out a few seconds later.
        assert_eq!(calls.viewer(secs(t, 30.0), true), Some(UiSound::ViewerJoined));
        assert_eq!(state(&mut calls, secs(t, 40.0), ANA, false, ROOM, ROOM, None, false), Some(UiSound::UserLeft));
        assert_eq!(calls.viewer(secs(t, 43.5), false), None);
        assert_eq!(calls.viewer(secs(t, 60.0), false), Some(UiSound::ViewerLeft));
        // The user's own stream ends: the viewers counted out after it are not news.
        assert_eq!(calls.own_stream(secs(t, 70.0), true), Some(UiSound::StreamStarted));
        assert_eq!(calls.viewer(secs(t, 71.0), true), Some(UiSound::ViewerJoined));
        assert_eq!(calls.own_stream(secs(t, 80.0), false), Some(UiSound::StreamEnded));
        assert_eq!(calls.viewer(secs(t, 80.3), false), None);
    }

    #[test]
    fn call_sounds_say_disconnected_once() {
        let t = Instant::now();
        let mut calls = in_room(t);
        let lost = |calls: &mut Calls, at: Instant| calls.disconnected(at).filter(|s| calls.allow(at, *s));
        assert_eq!(lost(&mut calls, secs(t, 10.0)), Some(UiSound::Disconnected));
        calls.requested(secs(t, 10.0), UiSound::Disconnected);
        // The server's removal and the voice connection closing are one disconnection; so
        // is a connection that drops again while it is being restored.
        assert_eq!(lost(&mut calls, secs(t, 10.2)), None);
        assert_eq!(lost(&mut calls, secs(t, 13.0)), None);
        // What the call still reports right after it is not announced.
        assert!(!calls.allow(secs(t, 10.5), UiSound::StreamEnded));
        assert_eq!(lost(&mut calls, secs(t, 60.0)), Some(UiSound::Disconnected));
        // Out of the channel: other people's states mean nothing until the user is back.
        assert_eq!(state(&mut calls, secs(t, 61.0), BO, false, None, ROOM, None, false), None);
    }
}
