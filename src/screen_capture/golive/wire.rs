//! The stream's own voice gateway connection (protocol version 8): what this client
//! sends, how the server's messages are read, and what a close code means.
//!
//! Version 8 differs from the version 4 that `gateway::voice` speaks: heartbeats are
//! objects that acknowledge the last sequence number seen, text messages may carry a
//! `seq`, and every binary message from the server starts with a 2-byte sequence number.
//!
//! ```text
//! -> 0 Identify            <- 8 Hello, 2 Ready
//! -> 1 Select Protocol     <- 4 Session Description
//! -> 5 Speaking, 12 Video  <- 11 / 13 users, 14 / 15 server wishes
//! -> 3 Heartbeat           <- 6 Heartbeat ACK
//! -> 7 Resume              <- 9 Resumed
//! 21 to 31: DAVE (see `dave`)
//! ```

use serde_json::{json, Value};

use super::dave::{OP_INVALID_COMMIT_WELCOME, OP_READY_FOR_TRANSITION};

pub const OP_IDENTIFY: u8 = 0;
pub const OP_SELECT_PROTOCOL: u8 = 1;
pub const OP_READY: u8 = 2;
pub const OP_HEARTBEAT: u8 = 3;
pub const OP_SESSION_DESCRIPTION: u8 = 4;
pub const OP_SPEAKING: u8 = 5;
pub const OP_HEARTBEAT_ACK: u8 = 6;
pub const OP_RESUME: u8 = 7;
pub const OP_HELLO: u8 = 8;
pub const OP_RESUMED: u8 = 9;
pub const OP_CLIENTS_CONNECT: u8 = 11;
pub const OP_VIDEO: u8 = 12;
pub const OP_CLIENT_DISCONNECT: u8 = 13;
pub const OP_SESSION_UPDATE: u8 = 14;
pub const OP_MEDIA_SINK_WANTS: u8 = 15;

/// Speaking flag of stream audio ("soundshare"): no speaking ring around the avatar.
pub const SPEAKING_SOUNDSHARE: u8 = 2;

/// The address of the stream gateway for the endpoint STREAM_SERVER_UPDATE named: only a
/// bare `host[:port]` is accepted (a leading `wss://` is dropped); anything with another
/// scheme, a path, a query or credentials is refused.
pub fn url(endpoint: &str, version: u8) -> Option<String> {
    let host = endpoint.trim();
    let host = host.strip_prefix("wss://").unwrap_or(host);
    let (name, port) = match host.rsplit_once(':') {
        Some((name, port)) => (name, Some(port)),
        None => (host, None),
    };
    let name_ok = !name.is_empty() && name.len() <= 253 && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-') && !name.starts_with(['.', '-']);
    let port_ok = port.map_or(true, |p| !p.is_empty() && p.len() <= 5 && p.bytes().all(|b| b.is_ascii_digit()) && p.parse::<u16>().is_ok_and(|p| p != 0));
    (name_ok && port_ok).then(|| format!("wss://{host}/?v={version}"))
}

/// Opcode 0. `channel_id` (the stream's `rtc_channel_id`) is part of version 9 only.
pub fn identify(server_id: u64, user_id: u64, session_id: &str, token: &str, channel_id: Option<u64>) -> Value {
    let mut d = json!({
        "server_id": server_id.to_string(),
        "user_id": user_id.to_string(),
        "session_id": session_id,
        "token": token,
        "video": true,
        "streams": [{ "type": "screen", "rid": "100", "quality": 100 }],
        "max_dave_protocol_version": davey02::DAVE_PROTOCOL_VERSION,
    });
    if let Some(channel) = channel_id {
        d["channel_id"] = json!(channel.to_string());
    }
    json!({ "op": OP_IDENTIFY, "d": d })
}

/// Opcode 7, on a new socket after the old one was lost: the session goes on.
pub fn resume(server_id: u64, session_id: &str, token: &str, seq_ack: i64, channel_id: Option<u64>) -> Value {
    let mut d = json!({
        "server_id": server_id.to_string(),
        "session_id": session_id,
        "token": token,
        "seq_ack": seq_ack,
    });
    if let Some(channel) = channel_id {
        d["channel_id"] = json!(channel.to_string());
    }
    json!({ "op": OP_RESUME, "d": d })
}

/// Opcode 3. `seq_ack`: the last sequence number received, -1 when none was.
pub fn heartbeat(unix_ms: u64, seq_ack: i64) -> Value {
    json!({ "op": OP_HEARTBEAT, "d": { "t": unix_ms, "seq_ack": seq_ack } })
}

/// Opcode 1: media goes over plain UDP with transport encryption `mode`, from the address
/// IP discovery found. Only Opus and H.264 are offered, so the server must answer H.264.
pub fn select_protocol(address: &str, port: u16, mode: &str, video_payload_type: u8, rtx: bool, rtc_connection_id: &str) -> Value {
    let mut h264 = json!({ "name": "H264", "type": "video", "priority": 1000, "payload_type": video_payload_type, "encode": true, "decode": true });
    if rtx {
        h264["rtx_payload_type"] = json!(video_payload_type + 1);
    }
    json!({
        "op": OP_SELECT_PROTOCOL,
        "d": {
            "protocol": "udp",
            "data": { "address": address, "port": port, "mode": mode },
            "codecs": [{ "name": "opus", "type": "audio", "priority": 1000, "payload_type": super::rtp::OPUS_PAYLOAD_TYPE }, h264],
            "rtc_connection_id": rtc_connection_id,
        }
    })
}

/// Opcode 5. Must be sent (with flags 0) before any media, and with the soundshare flag
/// before stream audio.
pub fn speaking(flags: u8, audio_ssrc: u32) -> Value {
    json!({ "op": OP_SPEAKING, "d": { "speaking": flags, "delay": 0, "ssrc": audio_ssrc } })
}

/// What opcode 12 announces about the video being sent.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VideoStream {
    pub ssrc: u32,
    /// 0: no retransmissions.
    pub rtx_ssrc: u32,
    /// "video" or "screen".
    pub kind: String,
    pub max_bitrate: u32,
    pub max_framerate: u32,
    pub width: u32,
    pub height: u32,
}

/// Opcode 12: the SSRCs in use and the stream sent; with `None`, that video stopped.
pub fn video(audio_ssrc: u32, stream: Option<&VideoStream>) -> Value {
    let d = match stream {
        None => json!({ "audio_ssrc": audio_ssrc, "video_ssrc": 0, "rtx_ssrc": 0, "streams": [] }),
        Some(s) => json!({
            "audio_ssrc": audio_ssrc,
            "video_ssrc": s.ssrc,
            "rtx_ssrc": s.rtx_ssrc,
            "streams": [{
                "type": s.kind,
                "rid": "100",
                "ssrc": s.ssrc,
                "active": true,
                "quality": 100,
                "rtx_ssrc": s.rtx_ssrc,
                "max_bitrate": s.max_bitrate,
                "max_framerate": s.max_framerate,
                "max_resolution": { "type": "fixed", "width": s.width, "height": s.height },
            }],
        }),
    };
    json!({ "op": OP_VIDEO, "d": d })
}

/// Opcode 23: ready for the DAVE transition the gateway prepared.
pub fn ready_for_transition(transition: u16) -> Value {
    json!({ "op": OP_READY_FOR_TRANSITION, "d": { "transition_id": transition } })
}

/// Opcode 31: the commit or welcome of this transition could not be processed.
pub fn invalid_commit_welcome(transition: u16) -> Value {
    json!({ "op": OP_INVALID_COMMIT_WELCOME, "d": { "transition_id": transition } })
}

/// A random version 4 UUID, for `rtc_connection_id`.
pub fn connection_id() -> String {
    let mut b: [u8; 16] = rand::random();
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    let hex: String = b.iter().map(|x| format!("{x:02x}")).collect();
    format!("{}-{}-{}-{}-{}", &hex[0..8], &hex[8..12], &hex[12..16], &hex[16..20], &hex[20..32])
}

/// A text message from the server.
#[derive(Clone, Debug, PartialEq)]
pub struct Text {
    pub op: u8,
    pub d: Value,
    /// The sequence number to acknowledge, when the message has one.
    pub seq: Option<i64>,
}

pub fn parse_text(text: &str) -> Option<Text> {
    let v: Value = serde_json::from_str(text).ok()?;
    Some(Text { op: u8::try_from(v["op"].as_u64()?).ok()?, seq: v["seq"].as_i64(), d: v.get("d").cloned().unwrap_or(Value::Null) })
}

/// A binary message from the server: `[sequence: u16][opcode][payload]`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Binary<'a> {
    pub seq: u16,
    pub op: u8,
    pub payload: &'a [u8],
}

pub fn parse_binary(frame: &[u8]) -> Option<Binary<'_>> {
    (frame.len() >= 3).then(|| Binary { seq: u16::from_be_bytes([frame[0], frame[1]]), op: frame[2], payload: &frame[3..] })
}

/// The payload of opcodes 29 and 30: a transition id, then the commit or welcome.
pub fn split_transition(payload: &[u8]) -> Option<(u16, &[u8])> {
    (payload.len() >= 2).then(|| (u16::from_be_bytes([payload[0], payload[1]]), &payload[2..]))
}

/// Opcode 2, Ready: where to send media, and which SSRCs to use.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Ready {
    pub audio_ssrc: u32,
    pub video_ssrc: u32,
    pub rtx_ssrc: u32,
    /// The video SSRCs came from Ready's `streams`; otherwise they follow the old
    /// convention (audio + 1 and + 2).
    pub ssrcs_from_streams: bool,
    pub ip: String,
    pub port: u16,
    pub modes: Vec<String>,
    pub experiments: Vec<String>,
}

fn strings(value: &Value) -> Vec<String> {
    value.as_array().map(|a| a.iter().filter_map(|s| s.as_str().map(str::to_string)).collect()).unwrap_or_default()
}

pub fn parse_ready(d: &Value) -> Option<Ready> {
    let audio_ssrc = u32::try_from(d["ssrc"].as_u64()?).ok()?;
    let ip = d["ip"].as_str().filter(|ip| !ip.is_empty())?.to_string();
    let port = u16::try_from(d["port"].as_u64()?).ok().filter(|p| *p != 0)?;
    let ssrc_of = |v: &Value| v.as_u64().and_then(|n| u32::try_from(n).ok()).filter(|n| *n != 0);
    // The stream this client named in Identify (rid "100"), else the first one.
    let streams = d["streams"].as_array().map(Vec::as_slice).unwrap_or_default();
    let stream = streams.iter().find(|s| s["rid"] == "100" && ssrc_of(&s["ssrc"]).is_some()).or_else(|| streams.iter().find(|s| ssrc_of(&s["ssrc"]).is_some()));
    let (video_ssrc, rtx_ssrc, ssrcs_from_streams) = match stream {
        Some(s) => {
            let video = ssrc_of(&s["ssrc"])?;
            (video, ssrc_of(&s["rtx_ssrc"]).unwrap_or(video.wrapping_add(1)), true)
        }
        None => (audio_ssrc.wrapping_add(1), audio_ssrc.wrapping_add(2), false),
    };
    Some(Ready { audio_ssrc, video_ssrc, rtx_ssrc, ssrcs_from_streams, ip, port, modes: strings(&d["modes"]), experiments: strings(&d["experiments"]) })
}

/// Opcode 4, Session Description: the transport key and what was negotiated.
#[derive(Clone, PartialEq, Eq)]
pub struct SessionDescription {
    pub mode: String,
    pub secret_key: [u8; 32],
    pub audio_codec: String,
    pub video_codec: String,
    pub dave_protocol_version: u16,
    pub media_session_id: String,
}

// (No Debug: the key must not reach a log line.)
impl std::fmt::Debug for SessionDescription {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "SessionDescription {{ mode: {}, video_codec: {}, dave_protocol_version: {} }}", self.mode, self.video_codec, self.dave_protocol_version)
    }
}

pub fn parse_session_description(d: &Value) -> Result<SessionDescription, String> {
    let key: Vec<u8> = d["secret_key"].as_array().ok_or("no secret_key")?.iter().filter_map(|b| b.as_u64().and_then(|b| u8::try_from(b).ok())).collect();
    let secret_key: [u8; 32] = key.try_into().map_err(|k: Vec<u8>| format!("secret_key of {} bytes", k.len()))?;
    let text = |name: &str| d[name].as_str().unwrap_or_default().to_string();
    Ok(SessionDescription {
        mode: text("mode"),
        secret_key,
        audio_codec: text("audio_codec"),
        video_codec: text("video_codec"),
        dave_protocol_version: d["dave_protocol_version"].as_u64().unwrap_or(0) as u16,
        media_session_id: text("media_session_id"),
    })
}

/// What to do when the stream gateway closes the socket with a code.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CloseAction {
    /// Open a new socket and resume the session: the server went away, not the session.
    Resume,
    /// The session is gone: open a new socket and identify again.
    Identify,
    /// The stream is over for this client; trying again cannot help.
    Fatal,
}

/// Voice gateway close codes (Discord's documentation of voice connections).
pub fn close_action(code: u16) -> CloseAction {
    match code {
        // Below 4000: the socket's own codes (going away, abnormal closure...). 4015: the
        // voice server crashed.
        0..=3999 | 4015 => CloseAction::Resume,
        // 4006 session no longer valid, 4009 session timed out.
        4006 | 4009 => CloseAction::Identify,
        // 4001 unknown opcode, 4002 payload not decoded, 4003 not authenticated, 4004
        // authentication failed, 4005 already authenticated, 4011 server not found, 4012
        // unknown protocol, 4014 disconnected (kicked, channel deleted), 4016 unknown
        // encryption mode, 4017 DAVE required, 4020 bad request, 4021 rate limited, 4022
        // call terminated.
        4001..=4005 | 4011 | 4012 | 4014 | 4016 | 4017 | 4020 | 4021 | 4022 => CloseAction::Fatal,
        // Codes nobody documents: a new session is the careful guess (retries are capped).
        _ => CloseAction::Identify,
    }
}

/// The name of a close code, for the log.
pub fn close_name(code: u16) -> &'static str {
    match code {
        1000 => "normal closure",
        1001 => "going away",
        1006 => "connection lost",
        4001 => "unknown opcode",
        4002 => "failed to decode payload",
        4003 => "not authenticated",
        4004 => "authentication failed",
        4005 => "already authenticated",
        4006 => "session no longer valid",
        4009 => "session timeout",
        4011 => "server not found",
        4012 => "unknown protocol",
        4014 => "disconnected",
        4015 => "voice server crashed",
        4016 => "unknown encryption mode",
        4017 => "end-to-end encryption (DAVE) required",
        4020 => "bad request",
        4021 => "rate limited",
        4022 => "call terminated",
        _ => "undocumented",
    }
}
