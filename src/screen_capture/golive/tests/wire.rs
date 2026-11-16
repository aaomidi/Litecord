//! T7: what goes to the main gateway and to the stream gateway, and how their messages
//! are read.

use serde_json::{json, Value};

use super::super::dave::GroupChoice;
use super::super::rtp::{Ext, ExtProfile, Playout, OMIT_TIMING, OMIT_TOFFSET};
use super::super::session::{KeyInt, Switches};
use super::super::signal::{self, Event, Region, StreamKey};
use super::super::wire::{self, CloseAction};

fn golden(text: &str) -> Value {
    serde_json::from_str(text).unwrap()
}

#[test]
fn golive_stream_keys_are_built_and_read() {
    let guild = StreamKey::new(Some("839502008108580904"), "850360749460553769", "852892297661906993");
    assert_eq!(guild.to_string(), "guild:839502008108580904:850360749460553769:852892297661906993");
    assert_eq!(StreamKey::parse(&guild.to_string()), Some(guild.clone()));
    let call = StreamKey::new(None, "1110739331624210483", "852892297661906993");
    assert_eq!(call.to_string(), "call:1110739331624210483:852892297661906993");
    assert_eq!(StreamKey::parse(&call.to_string()), Some(call.clone()));
    // A DM call's voice state has no server; the gateway client calls that "@me".
    assert_eq!(StreamKey::new(Some("@me"), "1", "2"), StreamKey::new(None, "1", "2"));
    assert_eq!(StreamKey::new(Some(""), "1", "2").to_string(), "call:1:2");
    assert!(guild.is_of(852892297661906993) && !guild.is_of(852892297661906994));
    assert!(signal::is_stream_of("call:1110739331624210483:852892297661906993", 852892297661906993));
    assert!(!signal::is_stream_of("guild:1:2:3", 4));
    for bad in ["", "guild:1:2", "guild:1:2:3:4", "call:1", "call:1:2:3", "test:852892297661906993", "guild:a:2:3", "guild:1::3", "call:1:-2", "Guild:1:2:3"] {
        assert_eq!(StreamKey::parse(bad), None, "{bad}");
        assert!(!signal::is_stream_of(bad, 3));
    }
}

#[test]
fn golive_main_gateway_payloads() {
    let guild = StreamKey::new(Some("111"), "222", "333");
    assert_eq!(signal::create(&guild, Region::Null), golden(r#"{"op":18,"d":{"type":"guild","guild_id":"111","channel_id":"222","preferred_region":null}}"#));
    assert_eq!(signal::create(&guild, Region::Omit), golden(r#"{"op":18,"d":{"type":"guild","guild_id":"111","channel_id":"222"}}"#));
    let call = StreamKey::new(None, "222", "333");
    assert_eq!(signal::create(&call, Region::Null), golden(r#"{"op":18,"d":{"type":"call","guild_id":null,"channel_id":"222","preferred_region":null}}"#));
    assert_eq!(signal::delete("guild:111:222:333"), golden(r#"{"op":19,"d":{"stream_key":"guild:111:222:333"}}"#));
    assert_eq!(signal::set_paused("call:222:333", false), golden(r#"{"op":22,"d":{"stream_key":"call:222:333","paused":false}}"#));
    assert_eq!(signal::set_paused("call:222:333", true)["d"]["paused"], json!(true));
}

#[test]
fn golive_stream_events_are_read() {
    let create = golden(r#"{"stream_key":"call:1110739331624210483:852892297661906993","rtc_server_id":"1278201813102755892","rtc_channel_id":"1278201813102755893","region":"us-east","viewer_ids":["193696591125807105"],"paused":false}"#);
    assert_eq!(
        signal::parse_event("STREAM_CREATE", &create),
        Some(Event::Create {
            stream_key: "call:1110739331624210483:852892297661906993".into(),
            rtc_server_id: 1278201813102755892,
            rtc_channel_id: Some(1278201813102755893),
            region: "us-east".into(),
            viewer_ids: vec![193696591125807105],
            paused: false,
        })
    );
    // Without the fields only STREAM_CREATE has, the same payload is a STREAM_UPDATE.
    let update = golden(r#"{"stream_key":"guild:1:2:3","region":"rotterdam","viewer_ids":["7","8"],"paused":true}"#);
    assert_eq!(signal::parse_event("STREAM_UPDATE", &update), Some(Event::Update { stream_key: "guild:1:2:3".into(), region: "rotterdam".into(), viewer_ids: vec![7, 8], paused: true }));
    assert_eq!(signal::parse_event("STREAM_CREATE", &update), None, "a stream cannot be connected to without its rtc_server_id");
    let server = golden(r#"{"token":"91f8016f34a5cd17","stream_key":"guild:1:2:3","guild_id":null,"endpoint":"smart.loyal.discord.media:1337"}"#);
    assert_eq!(
        signal::parse_event("STREAM_SERVER_UPDATE", &server),
        Some(Event::ServerUpdate { stream_key: "guild:1:2:3".into(), endpoint: Some("smart.loyal.discord.media:1337".into()), token: Some("91f8016f34a5cd17".into()) })
    );
    // The media server went away.
    let gone = golden(r#"{"token":"91f8016f34a5cd17","stream_key":"guild:1:2:3","endpoint":null}"#);
    assert!(matches!(signal::parse_event("STREAM_SERVER_UPDATE", &gone), Some(Event::ServerUpdate { endpoint: None, .. })));
    let delete = golden(r#"{"stream_key":"guild:1:2:3","reason":"stream_full"}"#);
    assert_eq!(signal::parse_event("STREAM_DELETE", &delete), Some(Event::Delete { stream_key: "guild:1:2:3".into(), reason: "stream_full".into(), unavailable: false }));
    let outage = golden(r#"{"stream_key":"guild:1:2:3","reason":"stream_ended","unavailable":true}"#);
    assert!(matches!(signal::parse_event("STREAM_DELETE", &outage), Some(Event::Delete { unavailable: true, .. })));
    assert_eq!(signal::parse_event("STREAM_DELETE", &outage).unwrap().stream_key(), "guild:1:2:3");
    // Not a stream event, and a stream event without its key.
    assert_eq!(signal::parse_event("VOICE_STATE_UPDATE", &delete), None);
    assert_eq!(signal::parse_event("STREAM_DELETE", &json!({ "reason": "x" })), None);
    assert_eq!(signal::parse_event("STREAM_CREATE", &Value::Null), None);
    // A key that is somebody else's parses (for watching, later) and is told apart.
    assert!(!signal::is_stream_of(signal::parse_event("STREAM_DELETE", &delete).unwrap().stream_key(), 4));
    assert!(signal::is_stream_of("guild:1:2:3", 3));
    assert_eq!(signal::EVENTS, ["STREAM_CREATE", "STREAM_SERVER_UPDATE", "STREAM_UPDATE", "STREAM_DELETE"]);
}

#[test]
fn golive_stream_gateway_payloads() {
    assert_eq!(
        wire::identify(1278201813102755892, 852892297661906993, "9a8b7c6d5e4f", "91f8016f34a5cd17", None),
        golden(
            r#"{"op":0,"d":{"server_id":"1278201813102755892","user_id":"852892297661906993","session_id":"9a8b7c6d5e4f","token":"91f8016f34a5cd17",
                "video":true,"streams":[{"type":"screen","rid":"100","quality":100}],"max_dave_protocol_version":1}}"#
        )
    );
    // Version 9 names the stream's RTC channel as well.
    assert_eq!(wire::identify(5, 6, "s", "t", Some(1278201813102755893))["d"]["channel_id"], json!("1278201813102755893"));
    assert_eq!(
        wire::select_protocol("203.0.113.7", 50123, "aead_aes256_gcm_rtpsize", 101, true, "0d2f9b7c-3c1e-4c55-9d0e-2f6c1a0b9e11"),
        golden(
            r#"{"op":1,"d":{"protocol":"udp","data":{"address":"203.0.113.7","port":50123,"mode":"aead_aes256_gcm_rtpsize"},
                "codecs":[{"name":"opus","type":"audio","priority":1000,"payload_type":120},
                          {"name":"H264","type":"video","priority":1000,"payload_type":101,"rtx_payload_type":102,"encode":true,"decode":true}],
                "rtc_connection_id":"0d2f9b7c-3c1e-4c55-9d0e-2f6c1a0b9e11"}}"#
        )
    );
    // Without retransmissions no RTX payload type is declared; the other payload type pair.
    let plain = wire::select_protocol("203.0.113.7", 50123, "aead_xchacha20_poly1305_rtpsize", 105, false, "x");
    assert_eq!(plain["d"]["codecs"][1], golden(r#"{"name":"H264","type":"video","priority":1000,"payload_type":105,"encode":true,"decode":true}"#));
    assert_eq!(wire::select_protocol("a", 1, "m", 105, true, "x")["d"]["codecs"][1]["rtx_payload_type"], json!(106));
    assert_eq!(wire::heartbeat(1_794_700_000_123, 42), golden(r#"{"op":3,"d":{"t":1794700000123,"seq_ack":42}}"#));
    assert_eq!(wire::heartbeat(5, -1), golden(r#"{"op":3,"d":{"t":5,"seq_ack":-1}}"#));
    assert_eq!(wire::speaking(0, 1001), golden(r#"{"op":5,"d":{"speaking":0,"delay":0,"ssrc":1001}}"#));
    assert_eq!(wire::speaking(wire::SPEAKING_SOUNDSHARE, 1001), golden(r#"{"op":5,"d":{"speaking":2,"delay":0,"ssrc":1001}}"#));
    assert_eq!(
        wire::resume(1278201813102755892, "9a8b7c6d5e4f", "91f8016f34a5cd17", 17, None),
        golden(r#"{"op":7,"d":{"server_id":"1278201813102755892","session_id":"9a8b7c6d5e4f","token":"91f8016f34a5cd17","seq_ack":17}}"#)
    );
    assert_eq!(wire::resume(5, "s", "t", -1, Some(9))["d"], golden(r#"{"server_id":"5","session_id":"s","token":"t","seq_ack":-1,"channel_id":"9"}"#));
    let stream = wire::VideoStream { ssrc: 1002, rtx_ssrc: 1003, kind: "video".into(), max_bitrate: 8_000_000, max_framerate: 60, width: 1920, height: 1080 };
    assert_eq!(
        wire::video(1001, Some(&stream)),
        golden(
            r#"{"op":12,"d":{"audio_ssrc":1001,"video_ssrc":1002,"rtx_ssrc":1003,"streams":[{"type":"video","rid":"100","ssrc":1002,"active":true,
                "quality":100,"rtx_ssrc":1003,"max_bitrate":8000000,"max_framerate":60,
                "max_resolution":{"type":"fixed","width":1920,"height":1080}}]}}"#
        )
    );
    assert_eq!(wire::video(1001, None), golden(r#"{"op":12,"d":{"audio_ssrc":1001,"video_ssrc":0,"rtx_ssrc":0,"streams":[]}}"#));
    assert_eq!(wire::ready_for_transition(7), golden(r#"{"op":23,"d":{"transition_id":7}}"#));
    assert_eq!(wire::invalid_commit_welcome(65_535), golden(r#"{"op":31,"d":{"transition_id":65535}}"#));
    // A version 4 UUID, another one each time.
    let id = wire::connection_id();
    let parts: Vec<&str> = id.split('-').collect();
    assert_eq!(parts.iter().map(|p| p.len()).collect::<Vec<_>>(), [8, 4, 4, 4, 12]);
    assert!(id.bytes().all(|b| b == b'-' || b.is_ascii_hexdigit()) && parts[2].starts_with('4') && "89ab".contains(&parts[3][..1]));
    assert_ne!(id, wire::connection_id());
}

#[test]
fn golive_stream_gateway_messages_are_read() {
    let hello = wire::parse_text(r#"{"op":8,"d":{"v":8,"heartbeat_interval":13750}}"#).unwrap();
    assert_eq!((hello.op, hello.seq, hello.d["heartbeat_interval"].as_u64()), (8, None, Some(13750)));
    let numbered = wire::parse_text(r#"{"op":11,"d":{"user_ids":["7"]},"seq":12}"#).unwrap();
    assert_eq!((numbered.op, numbered.seq), (11, Some(12)));
    assert_eq!(wire::parse_text(r#"{"op":9,"d":null}"#).unwrap().d, Value::Null);
    assert_eq!(wire::parse_text(r#"{"op":6}"#).unwrap().d, Value::Null);
    for bad in ["", "not json", r#"{"d":{}}"#, r#"{"op":"8"}"#, r#"{"op":300,"d":{}}"#, "[]"] {
        assert_eq!(wire::parse_text(bad), None, "{bad}");
    }
    // Binary: a 2-byte sequence number, the opcode, the payload.
    assert_eq!(wire::parse_binary(&[0x01, 0x02, 25, 9, 8, 7]), Some(wire::Binary { seq: 0x0102, op: 25, payload: &[9, 8, 7] }));
    assert_eq!(wire::parse_binary(&[0, 3, 27]), Some(wire::Binary { seq: 3, op: 27, payload: &[] }));
    assert_eq!(wire::parse_binary(&[0, 3]), None);
    assert_eq!(wire::split_transition(&[0x00, 0x07, 1, 2, 3]), Some((7, &[1u8, 2, 3][..])));
    assert_eq!(wire::split_transition(&[0xff, 0xff]), Some((65_535, &[][..])));
    assert_eq!(wire::split_transition(&[1]), None);

    let ready = golden(
        r#"{"ssrc":1001,"ip":"66.22.196.12","port":50007,"modes":["aead_aes256_gcm_rtpsize","aead_xchacha20_poly1305_rtpsize","xsalsa20_poly1305_lite_rtpsize"],
            "experiments":["fixed_keyframe_interval"],"streams":[{"type":"video","rid":"50","quality":50,"ssrc":2001,"rtx_ssrc":2002,"active":false},
            {"type":"video","rid":"100","quality":100,"ssrc":1002,"rtx_ssrc":1003,"active":false}]}"#,
    );
    let read = wire::parse_ready(&ready).unwrap();
    assert_eq!((read.audio_ssrc, read.video_ssrc, read.rtx_ssrc, read.ssrcs_from_streams), (1001, 1002, 1003, true), "the stream named in Identify, rid 100");
    assert_eq!((read.ip.as_str(), read.port, read.modes.len(), read.experiments.len()), ("66.22.196.12", 50007, 3, 1));
    // No rid 100: the first stream. No RTX SSRC: the video SSRC + 1.
    let other = golden(r#"{"ssrc":1001,"ip":"66.22.196.12","port":50007,"modes":[],"streams":[{"type":"screen","rid":"0","ssrc":3001}]}"#);
    assert_eq!(wire::parse_ready(&other).map(|r| (r.video_ssrc, r.rtx_ssrc, r.ssrcs_from_streams)), Some((3001, 3002, true)));
    // No streams at all: the old convention, and the caller is told.
    let none = golden(r#"{"ssrc":1001,"ip":"66.22.196.12","port":50007,"modes":["aead_aes256_gcm_rtpsize"]}"#);
    assert_eq!(wire::parse_ready(&none).map(|r| (r.video_ssrc, r.rtx_ssrc, r.ssrcs_from_streams)), Some((1002, 1003, false)));
    for bad in [r#"{"ip":"1.2.3.4","port":5}"#, r#"{"ssrc":1,"port":5}"#, r#"{"ssrc":1,"ip":"1.2.3.4"}"#, r#"{"ssrc":1,"ip":"","port":5}"#, r#"{"ssrc":1,"ip":"1.2.3.4","port":0}"#, r#"{"ssrc":1,"ip":"1.2.3.4","port":70000}"#] {
        assert_eq!(wire::parse_ready(&golden(bad)), None, "{bad}");
    }

    let key: Vec<u8> = (100..132).collect();
    let description = json!({ "audio_codec": "opus", "video_codec": "H264", "media_session_id": "abc", "mode": "aead_aes256_gcm_rtpsize", "secret_key": key, "dave_protocol_version": 1 });
    let read = wire::parse_session_description(&description).unwrap();
    assert_eq!((read.video_codec.as_str(), read.audio_codec.as_str(), read.mode.as_str(), read.dave_protocol_version, read.media_session_id.as_str()), ("H264", "opus", "aead_aes256_gcm_rtpsize", 1, "abc"));
    assert_eq!(read.secret_key.to_vec(), key);
    // The key never reaches a log line through Debug.
    assert!(!format!("{read:?}").contains("100") && !format!("{read:?}").contains("secret"));
    let mut without_dave = description.clone();
    without_dave.as_object_mut().unwrap().remove("dave_protocol_version");
    assert_eq!(wire::parse_session_description(&without_dave).unwrap().dave_protocol_version, 0);
    let mut short = description.clone();
    short["secret_key"] = json!([1, 2, 3]);
    assert!(wire::parse_session_description(&short).is_err());
    short["secret_key"] = json!((0..32).map(|i| i * 100).collect::<Vec<u32>>());
    assert!(wire::parse_session_description(&short).is_err(), "numbers that are not bytes");
    assert!(wire::parse_session_description(&json!({})).is_err());
}

#[test]
fn golive_close_codes_follow_the_table() {
    // The socket's own codes and a crashed voice server: resume.
    for code in [1000, 1001, 1005, 1006, 1011, 3999, 4015] {
        assert_eq!(wire::close_action(code), CloseAction::Resume, "{code}");
    }
    // The session is gone: identify again.
    for code in [4006, 4009] {
        assert_eq!(wire::close_action(code), CloseAction::Identify, "{code}");
    }
    // Trying again cannot help.
    for code in [4001, 4002, 4003, 4004, 4005, 4011, 4012, 4014, 4016, 4017, 4020, 4021, 4022] {
        assert_eq!(wire::close_action(code), CloseAction::Fatal, "{code}");
    }
    // Nobody documents these: a new session, with the retries capped.
    for code in [4000, 4007, 4008, 4010, 4013, 4018, 4019, 4023, 4999] {
        assert_eq!(wire::close_action(code), CloseAction::Identify, "{code}");
    }
    assert_eq!(wire::close_name(4017), "end-to-end encryption (DAVE) required");
    assert_eq!(wire::close_name(4014), "disconnected");
    assert_eq!(wire::close_name(4999), "undocumented");
}

#[test]
fn golive_only_a_bare_host_becomes_a_gateway_url() {
    assert_eq!(wire::url("smart.loyal.discord.media:1337", 8).as_deref(), Some("wss://smart.loyal.discord.media:1337/?v=8"));
    assert_eq!(wire::url("rotterdam1234.discord.media", 8).as_deref(), Some("wss://rotterdam1234.discord.media/?v=8"));
    assert_eq!(wire::url("wss://rotterdam1234.discord.media:443", 9).as_deref(), Some("wss://rotterdam1234.discord.media:443/?v=9"));
    assert_eq!(wire::url("  c-fra07-abcd.discord.media:2096 ", 8).as_deref(), Some("wss://c-fra07-abcd.discord.media:2096/?v=8"));
    assert_eq!(wire::url("127.0.0.1:4433", 8).as_deref(), Some("wss://127.0.0.1:4433/?v=8"));
    for bad in [
        "",
        "ws://host.discord.media",
        "http://host.discord.media",
        "https://host.discord.media/",
        "wss://wss://host",
        "host.discord.media/path",
        "host.discord.media/?v=4",
        "host.discord.media?v=4",
        "host.discord.media#x",
        "user@host.discord.media",
        "user:pw@host.discord.media:443",
        "host.discord.media:",
        "host.discord.media:0",
        "host.discord.media:99999",
        "host.discord.media:44a",
        ":443",
        "-host.discord.media",
        ".discord.media",
        "host discord.media",
        "host.discord.media\r\nX-Injected: 1",
        "[::1]:443",
    ] {
        assert_eq!(wire::url(bad, 8), None, "{bad:?}");
    }
}

#[test]
fn golive_switches_are_read_from_the_environment() {
    let from = |pairs: &[(&str, &str)]| {
        let pairs: Vec<(String, String)> = pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        Switches::read(move |name| pairs.iter().find(|(k, _)| k == name).map(|(_, v)| v.clone()))
    };
    let (defaults, unknown) = from(&[]);
    assert_eq!(defaults, Switches::default());
    assert!(unknown.is_empty());
    assert_eq!((defaults.ext, defaults.gateway_version, defaults.payload_type, defaults.rtx, defaults.stream_type), (ExtProfile::Full, 8, 101, true, "video"));
    assert_eq!((defaults.region, defaults.group, defaults.dave_check, defaults.require_dave, defaults.dump.is_none()), (Region::Null, GroupChoice::Auto, true, false, true));
    assert_eq!((defaults.audio_ext, defaults.idle_report), (false, false));
    // What the first live runs confirmed stays as it was; what they called for is new.
    assert_eq!((defaults.playout, defaults.ext_omit, defaults.keyint), (Playout::SMOOTH, 0, KeyInt::Seconds(60)));
    assert_eq!((defaults.even_timestamps, defaults.hold_ms, defaults.follow_remb, defaults.pace, defaults.max_kbps), (true, 20, true, 25, None));
    assert_eq!(defaults.ext(), Ext { profile: ExtProfile::Full, playout: Playout::Limits { min: 5, max: 1000 }, omit: 0 });
    assert_eq!(
        defaults.describe(),
        "EXT=full GW=8 PT=101 RTX=1 STREAM_TYPE=video REGION=null GROUP=auto DAVE_CHECK=1 REQUIRE_DAVE=0 AUDIO_EXT=none IDLE_SR=0 DUMP=off PLAYOUT=5,1000 EXT_OMIT=none KEYINT=60 TS=even HOLD=20 REMB=1 PACE=2.5 MAX_KBPS=share"
    );

    let (all, unknown) = from(&[
        ("LITECORD_GOLIVE_EXT", "min"),
        ("LITECORD_GOLIVE_GW", "9"),
        ("LITECORD_GOLIVE_PT", "105"),
        ("LITECORD_GOLIVE_RTX", "0"),
        ("LITECORD_GOLIVE_STREAM_TYPE", "screen"),
        ("LITECORD_GOLIVE_REGION", "omit"),
        ("LITECORD_GOLIVE_GROUP", "channel"),
        ("LITECORD_GOLIVE_DAVE_CHECK", "0"),
        ("LITECORD_GOLIVE_REQUIRE_DAVE", "1"),
        ("LITECORD_GOLIVE_AUDIO_EXT", "full"),
        ("LITECORD_GOLIVE_IDLE_SR", "1"),
        ("LITECORD_GOLIVE_DUMP", " /tmp/x "),
        ("LITECORD_GOLIVE_PLAYOUT", "0"),
        ("LITECORD_GOLIVE_EXT_OMIT", "timing,toffset"),
        ("LITECORD_GOLIVE_KEYINT", "share"),
        ("LITECORD_GOLIVE_TS", "clock"),
        ("LITECORD_GOLIVE_HOLD", "0"),
        ("LITECORD_GOLIVE_REMB", "0"),
        ("LITECORD_GOLIVE_PACE", "burst"),
        ("LITECORD_GOLIVE_MAX_KBPS", "6000"),
    ]);
    assert!(unknown.is_empty());
    assert_eq!((all.playout, all.ext_omit, all.keyint, all.even_timestamps, all.hold_ms), (Playout::ASAP, OMIT_TIMING | OMIT_TOFFSET, KeyInt::Share, false, 0));
    assert_eq!((all.follow_remb, all.pace, all.max_kbps), (false, 0, Some(6000)));
    assert_eq!((all.ext, all.gateway_version, all.payload_type, all.rtx, all.stream_type), (ExtProfile::Min, 9, 105, false, "screen"));
    assert_eq!((all.region, all.group, all.dave_check, all.require_dave), (Region::Omit, GroupChoice::Channel, false, true));
    assert_eq!(all.dump.as_deref(), Some(std::path::Path::new("/tmp/x")));
    assert_eq!((all.audio_ext, all.idle_report), (true, true));
    assert_eq!(
        all.describe(),
        "EXT=min GW=9 PT=105 RTX=0 STREAM_TYPE=screen REGION=omit GROUP=channel DAVE_CHECK=0 REQUIRE_DAVE=1 AUDIO_EXT=full IDLE_SR=1 DUMP=/tmp/x PLAYOUT=0,0 EXT_OMIT=toffset,timing KEYINT=share TS=clock HOLD=0 REMB=0 PACE=burst MAX_KBPS=6000"
    );
    let one = |name: &str, value: &str| from(&[(name, value)]).0;
    assert_eq!(one("LITECORD_GOLIVE_PLAYOUT", "none").playout, Playout::None);
    assert_eq!(one("LITECORD_GOLIVE_PLAYOUT", "10,1000").playout, Playout::Limits { min: 10, max: 1000 });
    assert_eq!(one("LITECORD_GOLIVE_EXT_OMIT", "none").ext_omit, 0);
    assert_eq!((one("LITECORD_GOLIVE_KEYINT", "0").keyint, one("LITECORD_GOLIVE_KEYINT", "5").keyint, one("LITECORD_GOLIVE_KEYINT", "server").keyint), (KeyInt::Seconds(0), KeyInt::Seconds(5), KeyInt::Server));
    assert_eq!((one("LITECORD_GOLIVE_PACE", "4").pace, one("LITECORD_GOLIVE_PACE", "1.5").pace, one("LITECORD_GOLIVE_TS", "even").even_timestamps), (40, 15, true));
    assert_eq!((one("LITECORD_GOLIVE_HOLD", "35").hold_ms, one("LITECORD_GOLIVE_HOLD", "100").hold_ms), (35, 100));
    // What the send loop is told about the periodic keyframe: 0 leaves its own rule,
    // the largest number is none at all.
    assert_eq!((KeyInt::Share.millis(Some(4000)), KeyInt::Seconds(0).millis(None), KeyInt::Seconds(60).millis(Some(4000))), (0, u64::MAX, 60_000));
    assert_eq!((KeyInt::Server.millis(Some(4000)), KeyInt::Server.millis(None), KeyInt::Server.millis(Some(0))), (4000, 60_000, 60_000));
    assert_eq!(from(&[("LITECORD_GOLIVE_EXT", "none")]).0.ext, ExtProfile::None);
    assert_eq!(from(&[("LITECORD_GOLIVE_GROUP", "server")]).0.group, GroupChoice::Server);
    assert_eq!(from(&[("LITECORD_GOLIVE_GROUP", "1278201813102755893")]).0.group, GroupChoice::Id(1278201813102755893));

    // Values that are not a switch's: the default stays, and the caller can say so.
    let (odd, unknown) = from(&[
        ("LITECORD_GOLIVE_EXT", "FULL"),
        ("LITECORD_GOLIVE_GW", "4"),
        ("LITECORD_GOLIVE_PT", "120"),
        ("LITECORD_GOLIVE_RTX", "yes"),
        ("LITECORD_GOLIVE_STREAM_TYPE", "camera"),
        ("LITECORD_GOLIVE_REGION", "us-east"),
        ("LITECORD_GOLIVE_GROUP", "-1"),
        ("LITECORD_GOLIVE_DAVE_CHECK", ""),
        ("LITECORD_GOLIVE_AUDIO_EXT", "1"),
        ("LITECORD_GOLIVE_IDLE_SR", "on"),
        ("LITECORD_GOLIVE_PLAYOUT", "10,5"),
        ("LITECORD_GOLIVE_EXT_OMIT", "playout"),
        ("LITECORD_GOLIVE_KEYINT", "never"),
        ("LITECORD_GOLIVE_TS", "cfr"),
        ("LITECORD_GOLIVE_HOLD", "101"),
        ("LITECORD_GOLIVE_REMB", "follow"),
        ("LITECORD_GOLIVE_PACE", "0.5"),
        ("LITECORD_GOLIVE_MAX_KBPS", "9000"),
    ]);
    assert_eq!(odd, Switches::default());
    assert_eq!(unknown.len(), 17, "{unknown:?} (an empty value is no value)");
    assert_eq!(from(&[("LITECORD_GOLIVE_PT", "119")]).0.payload_type, 101, "its RTX type would be Opus's");
    assert_eq!(from(&[("LITECORD_GOLIVE_PT", "200")]).0.payload_type, 101);
}

#[test]
fn golive_group_id_candidates() {
    use super::super::dave::{group_candidates, group_id_of_message, group_id_of_proposals};
    let ids = |choice| group_candidates(choice, 1000, 2000).iter().map(|c| c.0).collect::<Vec<_>>();
    assert_eq!(ids(GroupChoice::Auto), [999, 2000, 1000]);
    assert_eq!(group_candidates(GroupChoice::Auto, 1000, 2000)[0].1, "rtc_server_id - 1");
    assert_eq!(ids(GroupChoice::Channel), [2000, 999, 1000]);
    assert_eq!(ids(GroupChoice::Server), [1000, 999, 2000]);
    assert_eq!(ids(GroupChoice::Id(5)), [5, 999, 2000, 1000]);
    assert_eq!(ids(GroupChoice::Id(999)), [999, 2000, 1000]);
    assert_eq!(GroupChoice::from_name("auto"), Some(GroupChoice::Auto));
    assert_eq!(GroupChoice::from_name("12"), Some(GroupChoice::Id(12)));
    assert_eq!(GroupChoice::from_name("Channel"), None);
    // An MLS public message names its group in the clear: version 1, wire format 1, an
    // 8-byte id. In opcode 27 it sits behind the length of the vector it is in.
    let message = super::hex("0001 0001 08 11bd15c6dd400033 0000000000000000 02");
    assert_eq!(group_id_of_message(&message), Some(0x11bd15c6dd400033));
    assert_eq!(group_id_of_message(&message), Some(1278201813102755891));
    assert_eq!(group_id_of_proposals(&[&[0x41u8, 0xef][..], &message].concat()), Some(1278201813102755891));
    assert_eq!(group_id_of_proposals(&[&[0x15u8][..], &message].concat()), Some(1278201813102755891));
    assert_eq!(group_id_of_proposals(&[&[0x80u8, 0, 1, 0xef][..], &message].concat()), Some(1278201813102755891));
    assert_eq!(group_id_of_proposals(&[&[0xc0u8][..], &message].concat()), None);
    assert_eq!(group_id_of_message(&message[..12]), None);
    assert_eq!(group_id_of_message(&super::hex("0001 0002 08 11bd15c6dd400033 00")), None, "a private message");
    assert_eq!(group_id_of_message(&super::hex("0001 0001 10 11bd15c6dd400033 00")), None, "an id of another length");
    assert_eq!(group_id_of_proposals(&[]), None);
}
