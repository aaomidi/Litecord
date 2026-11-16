//! The main gateway's part of a Go Live stream: the stream key, the opcodes a client sends
//! (18 create, 19 delete, 22 pause or resume) and the STREAM_* events it gets back.
//!
//! ```text
//! -> {"op":18,"d":{"type":"guild","guild_id":"<guild>","channel_id":"<voice channel>","preferred_region":null}}
//! <- STREAM_CREATE        {"stream_key":"guild:<g>:<c>:<me>","rtc_server_id":"...","rtc_channel_id":"...","viewer_ids":[],"paused":false}
//! <- STREAM_SERVER_UPDATE {"stream_key":"...","token":"...","endpoint":"host[:port]"}
//! <- STREAM_UPDATE        {"stream_key":"...","viewer_ids":["..."],"paused":false}
//! -> {"op":19,"d":{"stream_key":"..."}}
//! <- STREAM_DELETE        {"stream_key":"...","reason":"user_requested"}
//! ```
//!
//! Events are parsed for any stream; milestone 1 acts on its own stream's only.

use serde_json::{json, Value};

pub const OP_STREAM_CREATE: u8 = 18;
pub const OP_STREAM_DELETE: u8 = 19;
pub const OP_STREAM_SET_PAUSED: u8 = 22;

/// The dispatch events of streams, for the gateway client's list of events it parses.
pub const EVENTS: [&str; 4] = ["STREAM_CREATE", "STREAM_SERVER_UPDATE", "STREAM_UPDATE", "STREAM_DELETE"];

/// `guild:{guild}:{channel}:{user}` in a server, `call:{channel}:{user}` in a DM or group call.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StreamKey {
    pub guild_id: Option<String>,
    pub channel_id: String,
    pub user_id: String,
}

impl StreamKey {
    pub fn new(guild_id: Option<&str>, channel_id: &str, user_id: &str) -> Self {
        Self { guild_id: guild_id.filter(|g| !g.is_empty() && *g != "@me").map(str::to_string), channel_id: channel_id.to_string(), user_id: user_id.to_string() }
    }

    pub fn parse(key: &str) -> Option<Self> {
        let parts: Vec<&str> = key.split(':').collect();
        let numeric = |s: &&str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
        match parts.as_slice() {
            ["guild", guild, channel, user] if [guild, channel, user].into_iter().all(numeric) => {
                Some(Self { guild_id: Some(guild.to_string()), channel_id: channel.to_string(), user_id: user.to_string() })
            }
            ["call", channel, user] if [channel, user].into_iter().all(numeric) => Some(Self { guild_id: None, channel_id: channel.to_string(), user_id: user.to_string() }),
            _ => None,
        }
    }

    /// The stream of user `user_id`.
    pub fn is_of(&self, user_id: u64) -> bool {
        self.user_id.parse() == Ok(user_id)
    }
}

impl std::fmt::Display for StreamKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.guild_id {
            Some(guild) => write!(f, "guild:{guild}:{}:{}", self.channel_id, self.user_id),
            None => write!(f, "call:{}:{}", self.channel_id, self.user_id),
        }
    }
}

/// Whether `stream_key` names a stream of user `user_id`.
pub fn is_stream_of(stream_key: &str, user_id: u64) -> bool {
    StreamKey::parse(stream_key).is_some_and(|k| k.is_of(user_id))
}

/// How opcode 18 states the preferred region (`LITECORD_GOLIVE_REGION`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Region {
    /// `"preferred_region": null`
    #[default]
    Null,
    /// The field is left out.
    Omit,
}

/// Opcode 18: create this account's stream in the voice channel it is in.
pub fn create(key: &StreamKey, region: Region) -> Value {
    let mut d = json!({
        "type": if key.guild_id.is_some() { "guild" } else { "call" },
        "guild_id": key.guild_id,
        "channel_id": key.channel_id,
    });
    if region == Region::Null {
        d["preferred_region"] = Value::Null;
    }
    json!({ "op": OP_STREAM_CREATE, "d": d })
}

/// Opcode 19: end the stream (its owner), or stop watching it.
pub fn delete(stream_key: &str) -> Value {
    json!({ "op": OP_STREAM_DELETE, "d": { "stream_key": stream_key } })
}

/// Opcode 22: pause or resume the stream.
pub fn set_paused(stream_key: &str, paused: bool) -> Value {
    json!({ "op": OP_STREAM_SET_PAUSED, "d": { "stream_key": stream_key, "paused": paused } })
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Event {
    Create { stream_key: String, rtc_server_id: u64, rtc_channel_id: Option<u64>, region: String, viewer_ids: Vec<u64>, paused: bool },
    /// `endpoint: None`: the stream's media server went away, another one will be named.
    ServerUpdate { stream_key: String, endpoint: Option<String>, token: Option<String> },
    Update { stream_key: String, region: String, viewer_ids: Vec<u64>, paused: bool },
    /// `unavailable`: an outage; the stream may come back with a new server.
    Delete { stream_key: String, reason: String, unavailable: bool },
}

impl Event {
    pub fn stream_key(&self) -> &str {
        match self {
            Event::Create { stream_key, .. } | Event::ServerUpdate { stream_key, .. } | Event::Update { stream_key, .. } | Event::Delete { stream_key, .. } => stream_key,
        }
    }
}

/// A snowflake, which Discord writes as a string (a number is accepted too).
pub fn snowflake(value: &Value) -> Option<u64> {
    value.as_str().and_then(|s| s.parse().ok()).or_else(|| value.as_u64())
}

fn snowflakes(value: &Value) -> Vec<u64> {
    value.as_array().map(|ids| ids.iter().filter_map(snowflake).collect()).unwrap_or_default()
}

fn text(value: &Value) -> String {
    value.as_str().unwrap_or_default().to_string()
}

/// The stream event of dispatch `t` with payload `d`; `None` for other events and for
/// payloads without what the event must have.
pub fn parse_event(t: &str, d: &Value) -> Option<Event> {
    let stream_key = d["stream_key"].as_str()?.to_string();
    match t {
        "STREAM_CREATE" => Some(Event::Create {
            stream_key,
            rtc_server_id: snowflake(&d["rtc_server_id"])?,
            rtc_channel_id: snowflake(&d["rtc_channel_id"]),
            region: text(&d["region"]),
            viewer_ids: snowflakes(&d["viewer_ids"]),
            paused: d["paused"].as_bool().unwrap_or(false),
        }),
        "STREAM_SERVER_UPDATE" => Some(Event::ServerUpdate {
            stream_key,
            endpoint: d["endpoint"].as_str().filter(|e| !e.trim().is_empty()).map(str::to_string),
            token: d["token"].as_str().filter(|t| !t.is_empty()).map(str::to_string),
        }),
        "STREAM_UPDATE" => Some(Event::Update { stream_key, region: text(&d["region"]), viewer_ids: snowflakes(&d["viewer_ids"]), paused: d["paused"].as_bool().unwrap_or(false) }),
        "STREAM_DELETE" => Some(Event::Delete { stream_key, reason: text(&d["reason"]), unavailable: d["unavailable"].as_bool().unwrap_or(false) }),
        _ => None,
    }
}
