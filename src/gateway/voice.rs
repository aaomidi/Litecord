#![allow(dead_code)]

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::collections::{HashMap, VecDeque};
use std::num::NonZeroU16;
use tokio::sync::{mpsc, Mutex};
use tokio::time::{sleep, Duration};
use tokio::net::UdpSocket;
use tokio_tungstenite::{connect_async, tungstenite::protocol::Message};
use futures_util::{SinkExt, StreamExt};
use serde_json::Value;
use log::{info, error, warn};
use davey::{DaveSession, ProposalsOperationType, MediaType};

use super::types::*;

use super::voice_tx::{take_mic_frame, TxInput, TxSettings, VoiceTx};
use super::voice_rx::{
    VoiceCipher, VoiceMixer, VoiceRxDecoder, RtpOpenError, push_voice_frame, select_voice_mode, smart_ducking,
    VOICE_MODE_AES256_GCM, VOICE_MODE_XCHACHA20, VOICE_QUEUE_LIMIT, VOICE_UNDERRUNS,
};

pub async fn connect_voice_gateway(
    raw_endpoint: &str,
    guild_id: &str,
    user_id: &str,
    session_id: &str,
    token: &str,
    channel_id: &str,
    self_mute_state: Arc<std::sync::Mutex<bool>>,
    _event_tx: mpsc::Sender<GatewayEvent>,
) {
    let clean_endpoint = raw_endpoint.trim();
    let voice_url = if clean_endpoint.starts_with("wss://") || clean_endpoint.starts_with("ws://") {
        clean_endpoint.to_string()
    } else {
        format!("wss://{}/?v=4", clean_endpoint)
    };
    info!("Conectando Ã  Discord Voice Gateway: {}...", voice_url);

    let my_session_id = CURRENT_VOICE_SESSION_ID.fetch_add(1, Ordering::SeqCst) + 1;
    info!("Iniciando nova sessÃ£o de voz ID={}", my_session_id);
    let cid_num: u64 = channel_id.parse().unwrap_or(0);
    set_my_voice_channel_id(cid_num);

    let mut is_first_connect = true;
    let mut final_close = false;
    loop {
        if CURRENT_VOICE_SESSION_ID.load(Ordering::SeqCst) != my_session_id || final_close {
            break;
        }

        if !is_first_connect {
            tokio::time::sleep(Duration::from_millis(1500)).await;
            if CURRENT_VOICE_SESSION_ID.load(Ordering::SeqCst) != my_session_id {
                break;
            }
            info!("🔄 [VOICE GATEWAY] Reconectando à Voice Gateway (sessão {})...", my_session_id);
        }
        is_first_connect = false;

        match connect_async(&voice_url).await {
            Ok((ws_stream, _)) => {
                info!("Conexão WebSocket com Voice Gateway estabelecida!");
                crate::screen_capture::invalidate_cached_stun_address();
                crate::screen_capture::force_signaling_broadcast();
                let (write, mut read) = ws_stream.split();
                let write_arc = Arc::new(Mutex::new(write));

                let guild_id = guild_id.to_string();
                let user_id = user_id.to_string();
                let session_id = session_id.to_string();
                let token = token.to_string();
                let channel_id_str = channel_id.to_string();
                sync_voice_channel_participants(&channel_id_str);
                let active_ssrc: Arc<std::sync::Mutex<u32>> = Arc::new(std::sync::Mutex::new(12345));
                let ssrc_to_userid: Arc<std::sync::Mutex<HashMap<u32, u64>>> = Arc::new(std::sync::Mutex::new(HashMap::new()));
                let secret_key_arc: Arc<std::sync::Mutex<Option<Vec<u8>>>> = Arc::new(std::sync::Mutex::new(None));

                // Create DAVE (Discord Audio/Video E2EE) session using the davey crate
                let uid_num: u64 = user_id.parse().unwrap_or(0);
                let cid_num: u64 = channel_id_str.parse().unwrap_or(0);
                let dave_session: Arc<std::sync::Mutex<Option<DaveSession>>> = Arc::new(std::sync::Mutex::new(
                    DaveSession::new(NonZeroU16::new(1).unwrap(), uid_num, cid_num, None)
                        .map_err(|e| { warn!("Falha ao criar DaveSession: {:?}", e); })
                        .ok()
                ));
                let saved_external_sender: Arc<std::sync::Mutex<Option<Vec<u8>>>> = Arc::new(std::sync::Mutex::new(None));
                let dave_flow = Arc::new(DaveFlow::new());

                // Read loop: wait for Opcode 8 HELLO from Voice Gateway before sending Opcode 0 Identify!
                while let Some(msg_result) = read.next().await {
                    if CURRENT_VOICE_SESSION_ID.load(Ordering::SeqCst) != my_session_id {
                        break;
                    }
                    match msg_result {
                        Ok(Message::Text(text)) => {
                            if let Ok(val) = serde_json::from_str::<Value>(&text) {
                                let op = val["op"].as_u64().unwrap_or(99);
                                info!("Payload recebido da Voice Gateway: op={}", op);

                            match op {
                                8 => {
                                    // Voice Opcode 8: HELLO
                                    let raw_interval = val["d"]["heartbeat_interval"]
                                        .as_f64()
                                        .unwrap_or(20000.0);
                                    let heartbeat_interval = (raw_interval * 0.75).max(1000.0) as u64;
                                    info!("Voice Gateway HELLO recebido! Intervalo de Heartbeat configurado: {} ms (servidor={}, fator=0.75)", heartbeat_interval, raw_interval);

                                    // 1. Send Opcode 0 Voice Identify (with max_dave_protocol_version: 1 for E2EE DAVE support)
                                    let voice_identify = serde_json::json!({
                                        "op": 0,
                                        "d": {
                                            "server_id": guild_id,
                                            "user_id": user_id,
                                            "session_id": session_id,
                                            "token": token,
                                            "video": false,
                                            "streams": [],
                                            "max_dave_protocol_version": 1
                                        }
                                    });

                                    info!("Enviando OP 0 Voice Identify para a Voice Gateway: {}", voice_identify);
                                    {
                                        let mut w = write_arc.lock().await;
                                        if let Err(e) = w.send(Message::Text(voice_identify.to_string().into())).await {
                                            error!("Erro ao enviar Opcode 0 Voice Identify: {:?}", e);
                                            break;
                                        }
                                    }

                                    // 2. Spawn Voice Heartbeat loop (Opcode 3 with epoch timestamp)
                                    let write_hb = Arc::clone(&write_arc);
                                    tokio::spawn(async move {
                                        loop {
                                            if CURRENT_VOICE_SESSION_ID.load(Ordering::SeqCst) != my_session_id { break; }
                                            sleep(Duration::from_millis(heartbeat_interval)).await;
                                            let t_now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_millis() as u64;
                                            let hb = serde_json::json!({ "op": 3, "d": t_now });
                                            let mut w = write_hb.lock().await;
                                            if let Err(e) = w.send(Message::Text(hb.to_string().into())).await {
                                                warn!("⚠️ Falha ao enviar Heartbeat de voz (op=3): {:?}", e);
                                                break;
                                            }
                                        }
                                    });
                                }
                                2 => {
                                    // Voice Opcode 2: READY!
                                    set_is_connected_to_voice(true);
                                    let ssrc = val["d"]["ssrc"].as_u64().unwrap_or(12345) as u32;
                                    *active_ssrc.lock().unwrap() = ssrc;
                                    ssrc_to_userid.lock().unwrap().insert(ssrc, uid_num);
                                    register_voice_participant(ssrc, uid_num);
                                    register_voice_participant(999999, 999999);

                                    let voice_ip = val["d"]["ip"].as_str().unwrap_or("").to_string();
                                    let voice_port = val["d"]["port"].as_u64().unwrap_or(0) as u16;

                                    // AES-256-GCM when the server offers it, otherwise XChaCha20-Poly1305
                                    // (always offered). Not simply the first of the list: the packets are
                                    // encrypted with the mode named here, whatever the list starts with.
                                    let offered_modes: Vec<&str> = val["d"]["modes"].as_array()
                                        .map(|modes| modes.iter().filter_map(|m| m.as_str()).collect())
                                        .unwrap_or_default();
                                    let selected_mode = match select_voice_mode(offered_modes.iter().copied()) {
                                        Some(mode) => mode.to_string(),
                                        None => {
                                            warn!("Voice: the server offers none of the supported encryption modes ({:?}); asking for {} anyway.", offered_modes, VOICE_MODE_AES256_GCM);
                                            VOICE_MODE_AES256_GCM.to_string()
                                        }
                                    };

                                    info!("🎉 VOICE GATEWAY PRONTA (Opcode 2 READY)! SSRC={}, IP={}:{}, Encryption Mode={}", ssrc, voice_ip, voice_port, selected_mode);

                                    // No chime here: joining already plays the quiet UiSound::JoinChannel
                                    // tone. (Upstream injected a raw 0.4 full-scale sine pair at this point,
                                    // on every voice READY, reconnects included.)
                                    if let Ok(mut queues) = get_speaker_pcm_queues().lock() {
                                        queues.entry(999999).or_insert_with(|| VecDeque::with_capacity(48000)).clear();
                                    }

                                    // UDP IP Discovery & Opcode 1 Select Protocol Handshake
                                    if !voice_ip.is_empty() && voice_port > 0 {
                                        let write_arc_proto = Arc::clone(&write_arc);
                                        let secret_key_udp = Arc::clone(&secret_key_arc);
                                        let dave_session_audio = Arc::clone(&dave_session);
                                        let dave_flow_audio = Arc::clone(&dave_flow);
                                        let dave_sender_audio = Arc::clone(&saved_external_sender);
                                        let ssrc_to_userid_audio = Arc::clone(&ssrc_to_userid);
                                        let self_mute_rx = Arc::clone(&self_mute_state);

                                        tokio::spawn(async move {
                                            let socket = match UdpSocket::bind("0.0.0.0:0").await {
                                                Ok(s) => Arc::new(s),
                                                Err(e) => { warn!("Falha ao criar socket UDP de voz: {:?}", e); return; }
                                            };
                                                let target_addr = format!("{}:{}", voice_ip, voice_port);
                                                info!("Socket UDP de Voz conectado a {}", target_addr);

                                                // 1. Send 74-byte UDP IP Discovery Packet (RFC / Discord spec: 2 bytes type, 2 bytes len, 4 bytes ssrc, 64 bytes addr, 2 bytes port)
                                                let mut discovery = [0u8; 74];
                                                discovery[0..2].copy_from_slice(&1u16.to_be_bytes());
                                                discovery[2..4].copy_from_slice(&70u16.to_be_bytes());
                                                discovery[4..8].copy_from_slice(&ssrc.to_be_bytes());

                                                let mut my_pub_ip = String::new();
                                                let mut my_pub_port = 0u16;

                                                for attempt in 1..=5 {
                                                    if socket.send_to(&discovery, &target_addr).await.is_ok() {
                                                        let mut buf = [0u8; 128];
                                                        if let Ok(Ok((len, _))) = tokio::time::timeout(Duration::from_millis(600), socket.recv_from(&mut buf)).await {
                                                            if len >= 70 {
                                                                let ip_slice = &buf[8..len.saturating_sub(2)];
                                                                let ip_end = ip_slice.iter().position(|&b| b == 0).unwrap_or(ip_slice.len());
                                                                let parsed_ip = String::from_utf8_lossy(&ip_slice[..ip_end]).trim().to_string();
                                                                let parsed_port = u16::from_be_bytes([buf[len - 2], buf[len - 1]]);

                                                                if !parsed_ip.is_empty() && parsed_port > 0 {
                                                                    my_pub_ip = parsed_ip;
                                                                    my_pub_port = parsed_port;
                                                                    info!("UDP IP Discovery resolvido com sucesso na tentativa {}: {}:{}", attempt, my_pub_ip, my_pub_port);
                                                                    break;
                                                                }
                                                            }
                                                        }
                                                    }
                                                    warn!("Tentativa {} de UDP IP Discovery falhou/timeout, tentando novamente...", attempt);
                                                }

                                                if my_pub_ip.is_empty() {
                                                    // Fallback to fetching public IP via HTTP if UDP discovery failed completely
                                                    if let Ok(res) = reqwest::get("https://api.ipify.org").await {
                                                        if let Ok(ip_text) = res.text().await {
                                                            let ip_trimmed = ip_text.trim().to_string();
                                                            if !ip_trimmed.is_empty() {
                                                                my_pub_ip = ip_trimmed;
                                                                my_pub_port = socket.local_addr().map(|a| a.port()).unwrap_or(50000);
                                                                warn!("Usando fallback HTTP para IP público: {}:{}", my_pub_ip, my_pub_port);
                                                            }
                                                        }
                                                    }
                                                }

                                                if my_pub_ip.is_empty() {
                                                    my_pub_ip = voice_ip.clone();
                                                    my_pub_port = socket.local_addr().map(|a| a.port()).unwrap_or(50000);
                                                    error!("CRÍTICO: Não foi possível determinar o IP público local!");
                                                }

                                                info!("UDP IP Discovery Concluído! IP: {}:{}", my_pub_ip, my_pub_port);

                                                // 2. Send Opcode 1 Select Protocol to Voice Gateway WebSocket
                                                let select_proto = serde_json::json!({
                                                    "op": 1,
                                                    "d": {
                                                        "protocol": "udp",
                                                        "data": {
                                                            "address": my_pub_ip,
                                                            "port": my_pub_port,
                                                            "mode": selected_mode
                                                        }
                                                    }
                                                });

                                                info!("Enviando Opcode 1 Select Protocol para a Voice Gateway...");
                                                {
                                                    let mut w = write_arc_proto.lock().await;
                                                    let _ = w.send(Message::Text(select_proto.to_string().into())).await;
                                                }


                                                // 4. Spawn incoming UDP voice receive loop
                                                let socket_rx = Arc::clone(&socket);
                                                let rx_mode = selected_mode.clone();
                                                let tx_mode = selected_mode.clone();
                                                let secret_key_rx = Arc::clone(&secret_key_udp);
                                                let dave_session_rx = Arc::clone(&dave_session_audio);
                                                let dave_flow_rx = Arc::clone(&dave_flow_audio);
                                                let dave_sender_rx = Arc::clone(&dave_sender_audio);
                                                let write_rx = Arc::clone(&write_arc_proto);
                                                let ssrc_to_userid_rx = Arc::clone(&ssrc_to_userid_audio);
                                                let speaker_queues_rx = get_speaker_pcm_queues();
                                                let my_ssrc = ssrc;
                                                let rx_session_id = my_session_id;
                                                tokio::spawn(async move {
                                                    let mut rx_decoders: HashMap<u32, VoiceRxDecoder> = HashMap::new();
                                                    let rx_clock = std::time::Instant::now();
                                                    let mut recv_buf = vec![0u8; 4096];
                                                    let mut detected_ssrcs = std::collections::HashSet::new();
                                                    let mut decrypt_err_cnt = 0u64;
                                                    let mut malformed_cnt = 0u64;
                                                    let mut padded_ok_cnt = 0u64;
                                                    let mut probe_cnt = 0u64;
                                                    let mut rx_cipher: Option<(Vec<u8>, VoiceCipher)> = None;
                                                    let mut last_prune_ms = 0u64;
                                                    let mut opus_err_cnt = 0u64;
                                                    let mut dave_decrypt_fail_cnt = 0u64;
                                                    let mut dave_plaintext_cnt = 0u64;
                                                    let mut _dave_not_ready_cnt = 0u64;

                                                    info!("🎧 Loop UDP de recepção de voz INICIADO (Session ID={})!", rx_session_id);

                                                    loop {
                                                        if CURRENT_VOICE_SESSION_ID.load(Ordering::SeqCst) != rx_session_id { break; }
                                                        match tokio::time::timeout(Duration::from_millis(100), socket_rx.recv_from(&mut recv_buf)).await {
                                                            Ok(Ok((len, addr))) => {
                                                                if len < 12 { continue; }
                                                                let pkt = &recv_buf[..len];
                                                                
                                                                // Parse RTP header
                                                                let version = (pkt[0] >> 6) & 0x3;
                                                                if version != 2 { continue; }
                                                                let pt = pkt[1] & 0x7F;
                                                                if pt != 120 { continue; } // Strictly allow ONLY Opus Audio packets (Payload Type 120)

                                                                let ssrc_recv = u32::from_be_bytes([pkt[8], pkt[9], pkt[10], pkt[11]]);
                                                                if ssrc_recv == my_ssrc { continue; }

                                                                let key_opt = secret_key_rx.lock().unwrap().clone();
                                                                if let Some(key_bytes) = key_opt {
                                                                    if rx_cipher.as_ref().map_or(true, |(k, _)| *k != key_bytes) {
                                                                        rx_cipher = VoiceCipher::new(&rx_mode, &key_bytes).map(|c| (key_bytes.clone(), c));
                                                                        if rx_cipher.is_none() {
                                                                            warn!("Voice RX: no cipher for mode '{}' with a {}-byte key: incoming voice cannot be decrypted.", rx_mode, key_bytes.len());
                                                                        }
                                                                    }
                                                                    if let Some((_, cipher)) = rx_cipher.as_ref() {
                                                                        // One layout for both modes (see VoiceCipher): header, CSRCs and the
                                                                        // extension preamble in the clear; extension body, payload and RTP
                                                                        // padding encrypted. The padding is removed here, before DAVE, which
                                                                        // looks for its marker at the very end of the frame.
                                                                        let has_padding = (pkt[0] & 0x20) != 0;
                                                                        let decrypted = match cipher.open(pkt) {
                                                                            Ok(payload) => payload,
                                                                            Err(RtpOpenError::Auth) => {
                                                                                decrypt_err_cnt += 1;
                                                                                if decrypt_err_cnt % 50 == 1 {
                                                                                    warn!("Voice transport decryption failed ({}) for SSRC {}: len={} padding={} header={:02X?} nonce={:02X?} ({} failures so far)",
                                                                                        cipher.mode(), ssrc_recv, len, has_padding, &pkt[..16.min(len)], &pkt[len - 4..], decrypt_err_cnt);
                                                                                }
                                                                                continue;
                                                                            }
                                                                            Err(e) => {
                                                                                malformed_cnt += 1;
                                                                                if malformed_cnt % 50 == 1 {
                                                                                    warn!("Voice RX: dropped a malformed packet ({:?}) from SSRC {}: len={} header={:02X?} ({} so far)",
                                                                                        e, ssrc_recv, len, &pkt[..16.min(len)], malformed_cnt);
                                                                                }
                                                                                continue;
                                                                            }
                                                                        };
                                                                        // Only now is the packet known to come from the voice server.
                                                                        if detected_ssrcs.insert(ssrc_recv) {
                                                                            info!("Voice RX: first packet from SSRC {} ({}), transport decryption OK ({}).", ssrc_recv, addr, cipher.mode());
                                                                        }
                                                                        if has_padding {
                                                                            padded_ok_cnt += 1;
                                                                            if padded_ok_cnt == 1 {
                                                                                info!("Voice RX: first packet with RTP padding decrypted (SSRC {}, len={}, payload {} bytes). These used to be dropped.", ssrc_recv, len, decrypted.len());
                                                                            }
                                                                        }
                                                                        if decrypted.is_empty() {
                                                                            // Padding only (a bandwidth probe): no audio in it.
                                                                            probe_cnt += 1;
                                                                            continue;
                                                                        }
                                                                        let transport_payload = &decrypted[..];

                                                                        let user_id_opt = ssrc_to_userid_rx.lock().unwrap().get(&ssrc_recv).copied();
                                                                        let sender_user_id = user_id_opt.unwrap_or(ssrc_recv as u64);

                                                                        let has_dave_magic = transport_payload.len() >= 4 && transport_payload.ends_with(&[0xFA, 0xFA]);
                                                                        let mut start_over = false;
                                                                        let (opus_data, can_decode) = {
                                                                            let mut sess = dave_session_rx.lock().unwrap();
                                                                            if let Some(ref mut s) = *sess {
                                                                                if s.is_ready() {
                                                                                    let res = s.decrypt(sender_user_id, MediaType::AUDIO, transport_payload);
                                                                                    let res = match res {
                                                                                        Ok(d) => Ok(d),
                                                                                        Err(_) => {
                                                                                            let swapped_uid = u64::from_be_bytes(sender_user_id.to_le_bytes());
                                                                                            s.decrypt(swapped_uid, MediaType::AUDIO, transport_payload)
                                                                                        }
                                                                                    };
                                                                                    let res = match res {
                                                                                        Ok(d) => Ok(d),
                                                                                        Err(_) => {
                                                                                            if let Some(uids) = s.get_user_ids() {
                                                                                                let mut found = None;
                                                                                                for &alt_uid in &uids {
                                                                                                    if alt_uid != s.user_id() {
                                                                                                        if let Ok(d) = s.decrypt(alt_uid, MediaType::AUDIO, transport_payload) {
                                                                                                            found = Some((d, alt_uid));
                                                                                                            break;
                                                                                                        }
                                                                                                    }
                                                                                                }
                                                                                                if let Some((d, matched_uid)) = found {
                                                                                                    if let Ok(mut m) = ssrc_to_userid_rx.lock() {
                                                                                                        m.insert(ssrc_recv, matched_uid);
                                                                                                    }
                                                                                                    register_voice_participant(ssrc_recv, matched_uid);
                                                                                                    Ok(d)
                                                                                                } else {
                                                                                                    Err(davey::errors::DecryptError::NoDecryptorForUser)
                                                                                                }
                                                                                            } else {
                                                                                                Err(davey::errors::DecryptError::NoDecryptorForUser)
                                                                                            }
                                                                                        }
                                                                                    };

                                                                                    match res {
                                                                                        Ok(d) => {
                                                                                            dave_flow_rx.failures.store(0, Ordering::Relaxed);
                                                                                            (d, true)
                                                                                        }
                                                                                        Err(_) if !has_dave_magic && dave_flow_rx.plaintext_allowed() => {
                                                                                            dave_plaintext_cnt += 1;
                                                                                            if dave_plaintext_cnt % 500 == 1 {
                                                                                                info!("🔓 [DAVE] Tocando pacotes sem DAVE durante a transição para a versão 0 ({} até agora).", dave_plaintext_cnt);
                                                                                            }
                                                                                            (transport_payload.to_vec(), true)
                                                                                        }
                                                                                        Err(e) => {
                                                                                            start_over = dave_flow_rx.failed_once();
                                                                                            #[cfg(feature = "automation")]
                                                                                            crate::utils::test_hooks::dave_decrypt_failed();
                                                                                            dave_decrypt_fail_cnt += 1;
                                                                                            if dave_decrypt_fail_cnt % 50 == 1 {
                                                                                                warn!("🔑 [DAVE DIAG] decrypt() FALHOU para SSRC={} UserID={} (payload_len={}, group_users={:?}): {:?}",
                                                                                                    ssrc_recv, sender_user_id, transport_payload.len(), s.get_user_ids(), e);
                                                                                            }
                                                                                            (Vec::new(), false)
                                                                                        }
                                                                                    }
                                                                                } else {
                                                                                    _dave_not_ready_cnt += 1;
                                                                                    if has_dave_magic {
                                                                                        (Vec::new(), false)
                                                                                    } else {
                                                                                        (transport_payload.to_vec(), true)
                                                                                    }
                                                                                }
                                                                            } else {
                                                                                if has_dave_magic {
                                                                                    (Vec::new(), false)
                                                                                } else {
                                                                                    (transport_payload.to_vec(), true)
                                                                                }
                                                                            }
                                                                        };
                                                                        if start_over {
                                                                            warn!("🔑 [DAVE] {} pacotes seguidos sem decifrar: pedindo um novo grupo ao servidor.", DAVE_FAILURE_TOLERANCE);
                                                                            tokio::spawn(dave_start_over(
                                                                                Arc::clone(&dave_flow_rx), Arc::clone(&dave_session_rx), Arc::clone(&dave_sender_rx),
                                                                                Arc::clone(&write_rx), uid_num, cid_num, protocol_version_now(&dave_session_rx),
                                                                            ));
                                                                        }

                                                                        if can_decode && !opus_data.is_empty() {
                                                                            let mut raw_opus = opus_data.as_slice();

                                                                            if raw_opus.len() >= 4 && (raw_opus.starts_with(&[0xBE, 0xDE]) || raw_opus.starts_with(&[0x10, 0x00])) {
                                                                                let ext_words = u16::from_be_bytes([raw_opus[2], raw_opus[3]]) as usize;
                                                                                let ext_total_bytes = 4 + ext_words * 4;
                                                                                if raw_opus.len() > ext_total_bytes {
                                                                                    raw_opus = &raw_opus[ext_total_bytes..];
                                                                                }
                                                                            }

                                                                            if raw_opus.first() == Some(&0x00) && raw_opus.len() > 1 {
                                                                                raw_opus = &raw_opus[1..];
                                                                            }

                                                                            if raw_opus.is_empty() { continue; }

                                                                            let rtp_seq = u16::from_be_bytes([pkt[2], pkt[3]]);
                                                                            let rtp_timestamp = u32::from_be_bytes([pkt[4], pkt[5], pkt[6], pkt[7]]);
                                                                            #[cfg(feature = "automation")]
                                                                            crate::utils::test_hooks::voice_received(rtp_seq);

                                                                            // Decode, with concealment of the packets lost before this one
                                                                            // (a new talkspurt after a pause conceals nothing).
                                                                            let now_ms = rx_clock.elapsed().as_millis() as u64;
                                                                            if now_ms.saturating_sub(last_prune_ms) > 60_000 {
                                                                                // Decoder state of members who left (about 0.5 MB each).
                                                                                last_prune_ms = now_ms;
                                                                                rx_decoders.retain(|ssrc, d| *ssrc == ssrc_recv || d.idle_ms(now_ms) < 300_000);
                                                                            }
                                                                            // How much of this member is still waiting to be played: decides
                                                                            // how many lost frames are worth concealing.
                                                                            let queued = speaker_queues_rx.lock().ok().map(|queues| queues.get(&ssrc_recv).map_or(0, |q| q.len()));
                                                                            let rx = rx_decoders.entry(ssrc_recv).or_insert_with(VoiceRxDecoder::new);
                                                                            let frame = rx.packet_for_playout(rtp_seq, rtp_timestamp, raw_opus, now_ms, queued);
                                                                            if let Some(e) = frame.error {
                                                                                opus_err_cnt += 1;
                                                                                if opus_err_cnt % 50 == 1 {
                                                                                    warn!("Voice RX: Opus decode failed for SSRC {} ({} bytes, first byte {:02X}): {} ({} failures so far)",
                                                                                        ssrc_recv, raw_opus.len(), raw_opus[0], e, opus_err_cnt);
                                                                                }
                                                                            }
                                                                            if !frame.decoded.is_empty() || !frame.concealed.is_empty() {
                                                                                register_voice_participant(ssrc_recv, sender_user_id);
                                                                                if let Ok(mut queues) = speaker_queues_rx.lock() {
                                                                                    let q = queues.entry(ssrc_recv).or_insert_with(|| VecDeque::with_capacity(VOICE_QUEUE_LIMIT));
                                                                                    #[cfg(feature = "automation")]
                                                                                    {
                                                                                        crate::utils::test_hooks::voice_decoded(&frame.concealed);
                                                                                        crate::utils::test_hooks::voice_decoded(&frame.decoded);
                                                                                    }
                                                                                    push_voice_frame(q, &frame.concealed, &frame.decoded);
                                                                                }
                                                                            }
                                                                            // What the sender's packets are and how the path treats them, for
                                                                            // comparing calls: after 5 s of speech, then every 30 s of it.
                                                                            let s = &rx.stats;
                                                                            if s.packets == 250 || s.packets % 1500 == 0 {
                                                                                info!("Voice RX stats SSRC={}: packets={} SILK={} hybrid={} CELT={} stereo={} with_fec={} silence_frames={} concealed={} concealed_as_silence={} conceal_failed={} conceal_skipped={} late_or_duplicate={} seq_jumps={} decode_errors={} decoder_panics={} | all members: padded_packets_ok={} transport_decrypt_failures={} malformed={} padding_only={} dave_decrypt_failures={} playout_underruns={}",
                                                                                    ssrc_recv, s.packets, s.silk, s.hybrid, s.celt, s.stereo, s.with_fec, s.silence_frames, s.concealed, s.concealed_as_silence, s.conceal_failed, s.conceal_skipped, s.late_dropped, s.seq_jumps, s.decode_errors, s.decoder_panics,
                                                                                    padded_ok_cnt, decrypt_err_cnt, malformed_cnt, probe_cnt, dave_decrypt_fail_cnt, VOICE_UNDERRUNS.load(Ordering::Relaxed));
                                                                            }
                                                                        }
                                                                            }
                                                                        }
                                                            }
                                                            Ok(Err(e)) => {
                                                                warn!("Erro socket.recv_from: {:?}", e);
                                                            }
                                                            Err(_) => { /* timeout 100ms, keep looping */ }
                                                        }
                                                    }
                                                    info!("Loop de recepção de voz (ID={}) encerrado.", rx_session_id);
                                                });

                                                let speaker_queues_out = get_speaker_pcm_queues();
                                                let ssrc_to_userid_speaker = Arc::clone(&ssrc_to_userid_audio);
                                                let out_session_id = my_session_id;
                                                std::thread::spawn(move || {
                                                    use cpal::traits::{HostTrait, DeviceTrait, StreamTrait};
                                                    let mut current_target_dev = get_selected_output_device_store().lock().unwrap().clone();
                                                    let mut no_device_wait: Option<Duration> = None;
                                                    let mut output_retry_wait: Option<Duration> = None;
                                                    let mut try_48k = true;
                                                    // Loads the per-member volume file now, not in the first callback.
                                                    let _ = get_user_audio_settings_store();

                                                    while CURRENT_VOICE_SESSION_ID.load(Ordering::SeqCst) == out_session_id {
                                                        let host = cpal::default_host();
                                                        let target_dev_name = current_target_dev.clone();
                                                        let device = if let Ok(devices) = host.output_devices() {
                                                            let dev_list: Vec<_> = devices.into_iter().collect();
                                                            if !target_dev_name.is_empty() && !target_dev_name.contains("Padrão") {
                                                                dev_list.into_iter().find(|d| d.name().map(|n| n == target_dev_name).unwrap_or(false))
                                                                    .or_else(|| host.default_output_device())
                                                            } else {
                                                                dev_list.into_iter().find(|d| {
                                                                    let n = d.name().unwrap_or_default();
                                                                    !n.contains("Litecord") && !n.contains("Virtual") && !n.contains("Null") && !n.contains("Steam")
                                                                }).or_else(|| host.default_output_device())
                                                            }
                                                        } else {
                                                            host.default_output_device()
                                                        };

                                                        #[cfg(feature = "automation")]
                                                        let device = device.filter(|_| !crate::utils::test_hooks::hide_output_device());
                                                        let device = match device {
                                                            Some(d) => d,
                                                            None => {
                                                                // Retry with backoff (each try enumerates every endpoint)
                                                                // and say it once, not every half second.
                                                                if no_device_wait.is_none() {
                                                                    warn!("Nenhum dispositivo de saída de áudio encontrado! Aguardando...");
                                                                }
                                                                let wait = next_no_device_wait(no_device_wait);
                                                                no_device_wait = Some(wait);
                                                                std::thread::sleep(wait);
                                                                continue;
                                                            }
                                                        };
                                                        if no_device_wait.take().is_some() {
                                                            info!("🔊 Dispositivo de saída de áudio disponível novamente.");
                                                        }

                                                        let default_config = match device.default_output_config() {
                                                            Ok(c) => c,
                                                            Err(e) => {
                                                                warn!("Falha ao obter config de saída: {:?}", e);
                                                                std::thread::sleep(std::time::Duration::from_millis(500));
                                                                continue;
                                                            }
                                                        };
                                                        // Opus decodes at 48 kHz. A device that offers that rate (PipeWire and
                                                        // PulseAudio do, whatever default cpal picks) plays the decoded samples
                                                        // as they are; any other rate goes through the resampler of VoiceMixer.
                                                        // Not on macOS: there the request changes the device's rate for every
                                                        // application (not tried on a Mac).
                                                        let config = if try_48k && !cfg!(target_os = "macos") {
                                                            let ranges = device.supported_output_configs().map(|r| r.collect::<Vec<_>>()).unwrap_or_default();
                                                            crate::audio::engine::voice_output_config_at_48k(default_config.clone(), ranges)
                                                        } else {
                                                            default_config.clone()
                                                        };
                                                        let asked_48k = config.sample_rate() != default_config.sample_rate();
                                                        let out_sample_rate = config.sample_rate().0;
                                                        let out_channels = config.channels() as usize;
                                                        info!("Saída de Áudio (Speaker): {}Hz, {} canal(is), formato={:?} (Dispositivo: {})",
                                                            out_sample_rate, out_channels, config.sample_format(), target_dev_name);
                                                        if asked_48k {
                                                            info!("Voice output: asked the device for 48000 Hz instead of its default {} Hz, so decoded voice is not resampled.", default_config.sample_rate().0);
                                                        } else if out_sample_rate != 48000 {
                                                            info!("Voice output: the device does not offer 48000 Hz; decoded voice is resampled to {} Hz.", out_sample_rate);
                                                        }

                                                    let sq = Arc::clone(&speaker_queues_out);
                                                    let ssrc_to_userid_spk = Arc::clone(&ssrc_to_userid_speaker);


                                                    #[cfg(not(target_os = "linux"))]
                                                    let stream_config: cpal::StreamConfig = config.clone().into();
                                                    #[cfg(target_os = "linux")]
                                                    let stream_config = crate::audio::engine::linux_voice_stream_config(&device, &config, false);

                                                    // Watchdog: the callbacks count how often they run and the error
                                                    // callback raises a flag; a stream that errors or goes quiet is rebuilt.
                                                    let output_ticks = Arc::new(std::sync::atomic::AtomicU64::new(0));
                                                    let output_broken = Arc::new(std::sync::atomic::AtomicBool::new(false));
                                                    let stream_res = match config.sample_format() {
                                                        cpal::SampleFormat::F32 => {
                                                            let ticks_f32 = Arc::clone(&output_ticks);
                                                            let broken_f32 = Arc::clone(&output_broken);
                                                            let sq_f32 = Arc::clone(&sq);
                                                            let ssrc_to_userid_f32 = Arc::clone(&ssrc_to_userid_spk);
                                                            let mut voice_out = VoiceOutput::new(out_sample_rate);
                                                            device.build_output_stream(
                                                                &stream_config,
                                                                move |output: &mut [f32], _| {
                                                                    ticks_f32.fetch_add(1, Ordering::Relaxed);
                                                                    #[cfg(feature = "automation")]
                                                                    if crate::utils::test_hooks::mute_output() {
                                                                        output.fill(0.0);
                                                                        return;
                                                                    }
                                                                    if CURRENT_VOICE_SESSION_ID.load(Ordering::SeqCst) != out_session_id {
                                                                        output.fill(0.0);
                                                                        return;
                                                                    }
                                                                    // Deafened: the voices are still played out, into nothing. Left
                                                                    // waiting they would be heard, 0.6 s old, when the output returns.
                                                                    let audible = !is_self_deaf();
                                                                    fill_voice_output(output, out_channels, &mut voice_out, &sq_f32, &ssrc_to_userid_f32, audible, |s| s);
                                                                    #[cfg(feature = "automation")]
                                                                    if audible {
                                                                        crate::utils::test_hooks::voice_played_f32(output);
                                                                    }
                                                                },
                                                                move |err| {
                                                                    warn!("Erro no stream de saída F32: {:?}", err);
                                                                    broken_f32.store(true, Ordering::Relaxed);
                                                                },
                                                                None,
                                                            )
                                                        }
                                                        cpal::SampleFormat::I16 => {
                                                            let ticks_i16 = Arc::clone(&output_ticks);
                                                            let broken_i16 = Arc::clone(&output_broken);
                                                            let sq_i16 = Arc::clone(&sq);
                                                            let ssrc_to_userid_i16 = Arc::clone(&ssrc_to_userid_spk);
                                                            let mut voice_out = VoiceOutput::new(out_sample_rate);
                                                            device.build_output_stream(
                                                                &stream_config,
                                                                move |output: &mut [i16], _| {
                                                                    ticks_i16.fetch_add(1, Ordering::Relaxed);
                                                                    #[cfg(feature = "automation")]
                                                                    if crate::utils::test_hooks::mute_output() {
                                                                        output.fill(0);
                                                                        return;
                                                                    }
                                                                    if CURRENT_VOICE_SESSION_ID.load(Ordering::SeqCst) != out_session_id {
                                                                        output.fill(0);
                                                                        return;
                                                                    }
                                                                    // Deafened: the voices are still played out, into nothing. Left
                                                                    // waiting they would be heard, 0.6 s old, when the output returns.
                                                                    let audible = !is_self_deaf();
                                                                    fill_voice_output(output, out_channels, &mut voice_out, &sq_i16, &ssrc_to_userid_i16, audible, |s| (s * 32767.0) as i16);
                                                                    #[cfg(feature = "automation")]
                                                                    if audible {
                                                                        crate::utils::test_hooks::voice_played_i16(output);
                                                                    }
                                                                },
                                                                move |err| {
                                                                    warn!("Erro no stream de saída I16: {:?}", err);
                                                                    broken_i16.store(true, Ordering::Relaxed);
                                                                },
                                                                None,
                                                            )
                                                        }
                                                        _ => {
                                                            warn!("Formato de saída não suportado: {:?}", config.sample_format());
                                                            std::thread::sleep(std::time::Duration::from_millis(1000));
                                                            continue;
                                                        }
                                                    };

                                                    match stream_res {
                                                        Ok(stream) => {
                                                            if let Err(e) = stream.play() {
                                                                warn!("Falha ao iniciar stream de saída: {:?}", e);
                                                                if asked_48k {
                                                                    info!("Voice output: the stream at 48000 Hz did not start; going back to the device's default rate.");
                                                                    try_48k = false;
                                                                }
                                                                std::thread::sleep(std::time::Duration::from_millis(500));
                                                                continue;
                                                            }
                                                            info!("🔊 Stream de Saída de Áudio (Speaker) ATIVO! Reproduzindo vozes dos outros usuários...");
                                                            #[cfg(feature = "automation")]
                                                            crate::utils::test_hooks::voice_output_started();
                                                            // Keep stream alive until voice session ends OR user selects another output device!
                                                            let mut last_ticks = 0u64;
                                                            let mut quiet_since = std::time::Instant::now();
                                                            loop {
                                                                std::thread::sleep(std::time::Duration::from_millis(100));
                                                                // A device that errors or stops asking for audio (driver reset,
                                                                // endpoint gone, exclusive mode taken) would leave the call silent.
                                                                let ticks = output_ticks.load(Ordering::Relaxed);
                                                                if ticks != last_ticks {
                                                                    last_ticks = ticks;
                                                                    quiet_since = std::time::Instant::now();
                                                                }
                                                                let broken = output_broken.load(Ordering::Relaxed);
                                                                if broken || quiet_since.elapsed() >= OUTPUT_STALL {
                                                                    warn!("🔊 Saída de áudio {}: recriando o stream.",
                                                                        if broken { "com erro" } else { "parada" });
                                                                    if asked_48k && ticks <= 500 {
                                                                        // It took 48000 Hz and then failed before playing for long:
                                                                        // the next stream uses the rate the device itself prefers.
                                                                        info!("Voice output: the stream at 48000 Hz failed early; going back to the device's default rate.");
                                                                        try_48k = false;
                                                                    }
                                                                    let wait = next_no_device_wait(output_retry_wait);
                                                                    output_retry_wait = Some(wait);
                                                                    drop(stream);
                                                                    std::thread::sleep(wait);
                                                                    break;
                                                                }
                                                                if ticks > 500 {
                                                                    // Has played for a while: the next failure retries quickly again.
                                                                    output_retry_wait = None;
                                                                }
                                                                // Test: the device stops the stream (as a failing driver would).
                                                                #[cfg(feature = "automation")]
                                                                if crate::utils::test_hooks::BREAK_VOICE_OUTPUT.swap(false, Ordering::SeqCst) {
                                                                    warn!("🧪 [TEST] Stream de saída interrompido (falha simulada do dispositivo).");
                                                                    let _ = stream.pause();
                                                                }
                                                                if CURRENT_VOICE_SESSION_ID.load(Ordering::SeqCst) != out_session_id {
                                                                    info!("Stream de saída encerrado (sessão expirada).");
                                                                    return;
                                                                }
                                                                let new_target = get_selected_output_device_store().lock().unwrap().clone();
                                                                if new_target != current_target_dev {
                                                                    info!("🔄 Dispositivo de saída alterado de '{}' para '{}' durante a chamada! Reconfigurando alto-falante...", current_target_dev, new_target);
                                                                    current_target_dev = new_target;
                                                                    try_48k = true; // another device: what the old one refused says nothing about it
                                                                    break; // Sai do loop interno, dropa o stream atual e o while externo reconstrói no novo dispositivo!
                                                                }
                                                            }
                                                        }
                                                        Err(e) => {
                                                            warn!("Falha ao criar stream de saída: {:?}", e);
                                                            if asked_48k {
                                                                info!("Voice output: the device refused 48000 Hz; going back to its default rate.");
                                                                try_48k = false;
                                                            }
                                                            std::thread::sleep(std::time::Duration::from_millis(500));
                                                        }
                                                    }
                                                }
                                                info!("Loop principal de áudio de saída encerrado.");
                                            });

                                                tokio::spawn(async move {
                                                let pcm_queue = get_mic_pcm_queue();
                                                let mut voice_tx = VoiceTx::new();
                                                let mut seq: u16 = 0;
                                                let mut timestamp: u32 = 0;
                                                let mut nonce_cnt: u32 = 0;
                                                let mut tx_cipher: Option<(Vec<u8>, VoiceCipher)> = None;
                                                let mut tx_turns: u32 = 0;
                                                let mut last_tx_turn = std::time::Instant::now();
                                                let mut speaking_loop_counter: u32 = 0;

                                                // Wait for the secret key (received in op=4) before starting audio
                                                info!("Aguardando chave secreta (op=4) antes de iniciar o áudio...");
                                                loop {
                                                    if CURRENT_VOICE_SESSION_ID.load(Ordering::SeqCst) != my_session_id { break; }
                                                    let has_key = secret_key_udp.lock().unwrap().is_some();
                                                    if has_key { break; }
                                                    sleep(Duration::from_millis(50)).await;
                                                }
                                                info!("Chave secreta recebida! Iniciando transmissão de áudio...");

                                                // Flush any stale mic buffer accumulated before voice handshake completed
                                                if let Ok(mut q) = pcm_queue.lock() {
                                                    q.clear();
                                                }

                                                // Stream AES-256-GCM Encrypted Opus microphone audio RTP frames every 20ms over UDP
                                                // One RTP frame per 20 ms of captured audio: the loop is paced by
                                                // the microphone (MIC_FRAME_READY) instead of a 20 ms timer. With
                                                // no mic data (no device, capture failed) a 60 ms timeout keeps the
                                                // silence keepalives going; it is long enough that a mic delivering
                                                // in large chunks never trips it in the middle of speech.
                                                // Returns false when it gave up waiting (no mic data).
                                                let next_frame = || async {
                                                    loop {
                                                        let ready = pcm_queue.lock().map(|q| q.len() >= 960).unwrap_or(false);
                                                        if ready {
                                                            return true;
                                                        }
                                                        tokio::select! {
                                                            _ = MIC_FRAME_READY.notified() => {}
                                                            _ = sleep(Duration::from_millis(60)) => return false,
                                                        }
                                                    }
                                                };
                                                // (The ten silence frames that open the route at the start of a call, the hangover and
                                                // the keepalives are in voice_tx.)

                                                // Test builds can switch back to the old timer pacing for A/B runs.
                                                #[cfg(feature = "automation")]
                                                let timer_pacing = crate::utils::test_hooks::timer_pacing();
                                                #[cfg(not(feature = "automation"))]
                                                let timer_pacing = false;
                                                let _fine_timer = timer_pacing.then(crate::utils::timer_resolution::FineTimerGuard::new);
                                                let mut pacing_timer = tokio::time::interval(Duration::from_millis(20));
                                                pacing_timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

                                                loop {
                                                    #[allow(unused_variables)]
                                                    let had_mic_frame = if timer_pacing {
                                                        pacing_timer.tick().await;
                                                        true
                                                    } else {
                                                        next_frame().await
                                                    };
                                                    if CURRENT_VOICE_SESSION_ID.load(Ordering::SeqCst) != my_session_id {
                                                        info!("Sessão de voz antiga (ID={}) encerrada, saindo do loop UDP!", my_session_id);
                                                        break;
                                                    }

                                                    // One turn of the sender (voice_tx): processing, automatic gain, the voice
                                                    // gate and the encoder. It says which packets to send and whether the turn
                                                    // took a packet slot (it does not while waiting out a late microphone in the
                                                    // middle of speech).
                                                    let is_muted = *self_mute_rx.lock().unwrap();
                                                    let (mic_frame, trimmed) = match pcm_queue.lock() {
                                                        Ok(mut q) => {
                                                            if is_muted {
                                                                q.clear();
                                                                (None, 0)
                                                            } else {
                                                                take_mic_frame(&mut q)
                                                            }
                                                        }
                                                        Err(_) => (None, 0),
                                                    };
                                                    voice_tx.note_trim(trimmed);
                                                    // Only turns that came with a frame: a wait for a late
                                                    // microphone is counted on its own.
                                                    if mic_frame.is_some() {
                                                        voice_tx.note_turn_gap(last_tx_turn.elapsed().as_millis() as u32);
                                                    }
                                                    last_tx_turn = std::time::Instant::now();
                                                    let suppression_mode = crate::utils::video_settings::get_noise_suppression();
                                                    let tx_settings = TxSettings {
                                                        noise_suppression: &suppression_mode,
                                                        echo_cancellation: crate::utils::video_settings::get_echo_cancellation(),
                                                        auto_gain: crate::utils::video_settings::get_automatic_gain_control(),
                                                        gate_threshold: get_vad_threshold(),
                                                    };
                                                    let tx_input = if is_muted {
                                                        TxInput::Muted
                                                    } else if let Some(frame) = mic_frame.as_ref() {
                                                        TxInput::Frame(frame)
                                                    } else {
                                                        TxInput::Stall
                                                    };
                                                    let turn = voice_tx.turn(tx_input, &tx_settings);

                                                    // The media timestamp moves on by one frame per packet slot (RFC 3550).
                                                    if turn.advance {
                                                        timestamp = timestamp.wrapping_add(960);
                                                    }

                                                    tx_turns = tx_turns.wrapping_add(1);
                                                    if tx_turns % 500 == 0 {
                                                        info!("Voice TX (10 s): {} | {}", voice_tx.take_report(), crate::audio::engine::take_capture_report());
                                                    }

                                                    let key_opt = secret_key_udp.lock().unwrap().clone();
                                                    let mut send_failed = false;
                                                    for packet in &turn.packets {
                                                        seq = seq.wrapping_add(1);
                                                        nonce_cnt = nonce_cnt.wrapping_add(1);

                                                        // Frames from before the gate opened go out with the timestamp of
                                                        // the slot they were captured in.
                                                        let packet_timestamp = timestamp.wrapping_sub(960 * packet.slots_back);
                                                        let mut audio_header = [0u8; 12];
                                                        audio_header[0] = 0x80; // RTP Version 2
                                                        audio_header[1] = 0x78; // Opus Payload 120
                                                        audio_header[2..4].copy_from_slice(&seq.to_be_bytes());
                                                        audio_header[4..8].copy_from_slice(&packet_timestamp.to_be_bytes());
                                                        audio_header[8..12].copy_from_slice(&ssrc.to_be_bytes());

                                                        let opus_bytes: &[u8] = &packet.opus;

                                                        // Apply DAVE frame encryption if session is ready
                                                        let dave_encrypted: Option<Vec<u8>> = {
                                                            let mut sess = dave_session_audio.lock().unwrap();
                                                            if let Some(ref mut s) = *sess {
                                                                if s.is_ready() {
                                                                    match s.encrypt_opus(opus_bytes) {
                                                                        Ok(cow) => Some(cow.into_owned()),
                                                                        Err(e) => {
                                                                            if speaking_loop_counter % 250 == 0 {
                                                                                warn!("DAVE: Falha em encrypt_opus: {:?}", e);
                                                                            }
                                                                            None
                                                                        }
                                                                    }
                                                                } else { None }
                                                            } else { None }
                                                        };
                                                        let opus_payload: &[u8] = if let Some(ref v) = dave_encrypted { v } else { opus_bytes };

                                                        let Some(key_bytes) = key_opt.as_ref() else { continue };
                                                        if tx_cipher.as_ref().map_or(true, |(k, _)| k != key_bytes) {
                                                            tx_cipher = VoiceCipher::new(&tx_mode, key_bytes).map(|c| (key_bytes.clone(), c));
                                                            if tx_cipher.is_none() {
                                                                warn!("Voice TX: no cipher for mode '{}' with a {}-byte key: nothing can be sent.", tx_mode, key_bytes.len());
                                                            }
                                                        }
                                                        let Some((_, cipher)) = tx_cipher.as_ref() else { continue };
                                                        // Transport encryption of the (DAVE-encrypted) Opus payload: the
                                                        // header is authenticated, the 4-byte nonce counter is appended.
                                                        let Some(rtp_pkt) = cipher.seal(&audio_header, opus_payload, nonce_cnt) else { continue };
                                                        if socket.send_to(&rtp_pkt, &target_addr).await.is_err() {
                                                            send_failed = true;
                                                            break;
                                                        }
                                                        #[cfg(feature = "automation")]
                                                        crate::utils::test_hooks::voice_sent(packet.audio, !had_mic_frame);

                                                        // Resend OP 5 Speaking every 250 packets (about 5 s of speech)
                                                        speaking_loop_counter = speaking_loop_counter.wrapping_add(1);
                                                        if speaking_loop_counter % 250 == 0 {
                                                            let speaking_pkt = serde_json::json!({
                                                                "op": 5,
                                                                "d": { "speaking": 1, "delay": 0, "ssrc": ssrc }
                                                            });
                                                            let mut w = write_arc_proto.lock().await;
                                                            let _ = w.send(Message::Text(speaking_pkt.to_string().into())).await;
                                                        }
                                                    }
                                                    if send_failed {
                                                        break;
                                                    }
                                                }
                                            });
                                        });
                                    }
                                }
                                4 => {
                                    // Voice Opcode 4: Session Description (Handshake Complete & Secret Key received!)
                                    let ssrc = *active_ssrc.lock().unwrap();
                                    info!("ðŸŽ‰ VOICE GATEWAY OPCODE 4 SESSION DESCRIPTION RECEBIDO! SessÃ£o de Voz Ativada com Sucesso!");

                                    match val["d"]["mode"].as_str() {
                                        Some(mode) if mode == VOICE_MODE_AES256_GCM || mode == VOICE_MODE_XCHACHA20 =>
                                            info!("Voice: the server confirmed transport encryption mode {}.", mode),
                                        other => warn!("Voice: the server's session description names encryption mode {:?}, which is not supported; expect decryption failures.", other),
                                    }
                                    if let Some(key_arr) = val["d"]["secret_key"].as_array() {
                                        let key_bytes: Vec<u8> = key_arr.iter().filter_map(|v| v.as_u64().map(|n| n as u8)).collect();
                                        if key_bytes.len() == 32 {
                                            info!("Chave de criptografia AES-256 de 32 bytes configurada com SUCESSO para os pacotes de áudio!");
                                            let mut fixed = [0u8; 32];
                                            fixed.copy_from_slice(&key_bytes);
                                            *get_voice_secret_key_store().lock().unwrap() = Some(fixed);
                                            *secret_key_arc.lock().unwrap() = Some(key_bytes);
                                        }
                                    }

                                    // Send Voice Opcode 5: Speaking
                                    let speaking_pkt = serde_json::json!({
                                        "op": 5,
                                        "d": {
                                            "speaking": 1,
                                            "delay": 0,
                                            "ssrc": ssrc
                                        }
                                    });

                                    info!("Enviando OP 5 Speaking para a Voice Gateway...");
                                    let mut w = write_arc.lock().await;
                                    let _ = w.send(Message::Text(speaking_pkt.to_string().into())).await;
                                }
                                 5 => {
                                     // Voice Opcode 5: Speaking notification from another user
                                     let s_ssrc = val["d"]["ssrc"].as_u64().map(|n| n as u32).unwrap_or(0);
                                     let u_id: u64 = if let Some(s) = val["d"]["user_id"].as_str() {
                                         s.parse().unwrap_or(0)
                                     } else {
                                         val["d"]["user_id"].as_u64().unwrap_or(0)
                                     };
                                     if s_ssrc > 0 && u_id > 0 {
                                         ssrc_to_userid.lock().unwrap().insert(s_ssrc, u_id);
                                         register_voice_participant(s_ssrc, u_id);
                                         info!("Voice Gateway OP 5: Mapeado SSRC {} -> User ID {}", s_ssrc, u_id);
                                     }
                                 }
                                 6 => {
                                     // Voice Opcode 6: Heartbeat ACK!
                                     info!("Voice Gateway Heartbeat ACK (Opcode 6) recebido!");
                                 }
                                 11 | 12 => {
                                     // Voice Opcode 11/12: User Joined / Client Connect
                                     if let Some(arr) = val["d"]["user_ids"].as_array() {
                                         for v in arr {
                                             if let Some(uid_str) = v.as_str() {
                                                 if let Ok(uid) = uid_str.parse::<u64>() {
                                                     let ssrc = (uid & 0xFFFFFFFF) as u32;
                                                     register_voice_participant(ssrc, uid);
                                                     info!("Voice Gateway OP 11/12 (user_ids): Registrado participante User ID {}", uid);
                                                 }
                                             }
                                         }
                                     }
                                     let s_ssrc = val["d"]["audio_ssrc"].as_u64().or_else(|| val["d"]["ssrc"].as_u64()).unwrap_or(0) as u32;
                                     let u_id: u64 = if let Some(s) = val["d"]["user_id"].as_str() {
                                         s.parse().unwrap_or(0)
                                     } else {
                                         val["d"]["user_id"].as_u64().unwrap_or(0)
                                     };
                                     if s_ssrc > 0 && u_id > 0 {
                                         ssrc_to_userid.lock().unwrap().insert(s_ssrc, u_id);
                                         register_voice_participant(s_ssrc, u_id);
                                         info!("Voice Gateway OP 11/12: Mapeado SSRC {} -> User ID {}", s_ssrc, u_id);
                                     }
                                 }
                                18 => {
                                    // DAVE Opcode 18: DAVE_PREPARE_TRANSITION (Sent for all active voice channel participants)
                                    let transition_id = val["d"]["transition_id"].as_u64().unwrap_or(0);
                                    let protocol_version = val["d"]["protocol_version"].as_u64().unwrap_or(99);
                                    let u_id: u64 = if let Some(s) = val["d"]["user_id"].as_str() {
                                        s.parse().unwrap_or(0)
                                    } else {
                                        val["d"]["user_id"].as_u64().unwrap_or(0)
                                    };
                                    if u_id > 0 {
                                        let ssrc = (u_id & 0xFFFFFFFF) as u32;
                                        register_voice_participant(ssrc, u_id);
                                        info!("👥 Voice Gateway OP 18 DAVE Transition: Registrado participante no canal de voz! User ID {}", u_id);
                                    }
                                    info!("DAVE Prepare Transition (op=18): transition_id={}, protocol_version={}, payload={}",
                                        transition_id, protocol_version, val["d"]);

                                    let ready = serde_json::json!({
                                        "op": 23,
                                        "d": { "transition_id": transition_id }
                                    });
                                    let mut w = write_arc.lock().await;
                                    let _ = w.send(Message::Text(ready.to_string().into())).await;
                                    info!("DAVE: Ready For Transition (op=23) enviado para transition_id={}", transition_id);
                                }
                                19 => {
                                    // DAVE Opcode 19: DAVE_EXECUTE_TRANSITION (Server -> Client)
                                    let transition_id = val["d"]["transition_id"].as_u64().unwrap_or(0);
                                    info!("DAVE Execute Transition (op=19): transition_id={}, payload={}", transition_id, val["d"]);
                                    let ssrc = *active_ssrc.lock().unwrap();
                                    let speaking_pkt = serde_json::json!({
                                        "op": 5,
                                        "d": { "speaking": 1, "delay": 0, "ssrc": ssrc }
                                    });
                                    let mut w = write_arc.lock().await;
                                    let _ = w.send(Message::Text(speaking_pkt.to_string().into())).await;
                                    info!("DAVE OP 19: Enviado OP 5 Speaking para sincronizar SSRC {} na transição de época!", ssrc);
                                }
                                20 => {
                                    // DAVE Opcode 20: DAVE_TRANSITION_READY
                                }
                                21 => {
                                    // DAVE Opcode 21: DAVE_PREPARE_TRANSITION (Server -> Client)
                                    let transition_id = val["d"]["transition_id"].as_u64().unwrap_or(0);
                                    let protocol_version = val["d"]["protocol_version"].as_u64().unwrap_or(0);
                                    info!("DAVE Prepare Transition (op=21): transition_id={}, protocol_version={}", transition_id, protocol_version);
                                    dave_flow.prepare_protocol(transition_id, protocol_version);
                                    let ready = serde_json::json!({
                                        "op": 23,
                                        "d": { "transition_id": transition_id }
                                    });
                                    let mut w = write_arc.lock().await;
                                    let _ = w.send(Message::Text(ready.to_string().into())).await;
                                    info!("DAVE: Ready For Transition (op=23) enviado para op=21 transition_id={}", transition_id);
                                }
                                22 => {
                                    // DAVE Opcode 22: DAVE_EXECUTE_TRANSITION (Server -> Client)
                                    let transition_id = val["d"]["transition_id"].as_u64().unwrap_or(0);
                                    info!("DAVE Execute Transition (op=22): transition_id={}, payload={}", transition_id, val["d"]);
                                    dave_flow.execute(transition_id);
                                    let ssrc = *active_ssrc.lock().unwrap();
                                    let speaking_pkt = serde_json::json!({
                                        "op": 5,
                                        "d": { "speaking": 1, "delay": 0, "ssrc": ssrc }
                                    });
                                    let mut w = write_arc.lock().await;
                                    let _ = w.send(Message::Text(speaking_pkt.to_string().into())).await;
                                    info!("DAVE OP 22: Enviado OP 5 Speaking para sincronizar SSRC {} na transição de época!", ssrc);
                                }
                                24 => {
                                    // DAVE Opcode 24: DAVE_PREPARE_EPOCH (Server -> Client)
                                    let epoch = val["d"]["epoch"].as_u64().unwrap_or(0);
                                    let protocol_version = val["d"]["protocol_version"].as_u64().unwrap_or(1);
                                    info!("DAVE Prepare Epoch (op=24): epoch={}, protocol_version={}", epoch, protocol_version);
                                    if epoch == 1 {
                                        dave_flow.failures.store(0, Ordering::Relaxed);
                                        let key_pkg_bytes = {
                                            let mut sess = dave_session.lock().unwrap();
                                            if let Some(ref mut s) = *sess {
                                                let _ = s.reset();
                                                if let Ok(mut new_s) = DaveSession::new(
                                                    NonZeroU16::new(protocol_version as u16).unwrap_or(NonZeroU16::new(1).unwrap()),
                                                    uid_num, cid_num, None
                                                ) {
                                                    if let Some(ref ext_bytes) = saved_external_sender.lock().unwrap().as_ref() {
                                                        let _ = new_s.set_external_sender(ext_bytes);
                                                    }
                                                    let kp = new_s.create_key_package().ok();
                                                    *s = new_s;
                                                    kp
                                                } else { None }
                                            } else { None }
                                        };
                                        if let Some(kp) = key_pkg_bytes {
                                            let mut pkt = vec![26u8];
                                            pkt.extend_from_slice(&kp);
                                            let mut w = write_arc.lock().await;
                                            let _ = w.send(Message::Binary(pkt.into())).await;
                                            info!("DAVE: Novo KeyPackage (op=26) gerado e enviado para Epoch Reset (op=24)!");
                                        }
                                    }
                                }
                                _ => {
                                    info!("Voice Gateway opcode JSON ignorado: op={}, data={}", op, val["d"]);
                                }
                            }
                        }
                    }
                    Ok(Message::Binary(data)) => {
                        if data.is_empty() { continue; }
                        let dave_op = data[0];
                        let payload = if data.len() > 1 { &data[1..] } else { &[][..] };

                        let preview: String = data[..data.len().min(16)].iter().map(|b| format!("{:02X}", b)).collect::<Vec<_>>().join(" ");
                        info!("Voice Gateway BINARY: dave_op={}, {} bytes total, preview=[{}]", dave_op, data.len(), preview);

                        match dave_op {
                            25 => {
                                // dave_mls_external_sender_package (25): gateway's MLS credential
                                // Process it, then send our KeyPackage (opcode 26)
                                *saved_external_sender.lock().unwrap() = Some(payload.to_vec());
                                let key_pkg_bytes = {
                                    let mut sess = dave_session.lock().unwrap();
                                    if let Some(ref mut s) = *sess {
                                        match s.set_external_sender(payload) {
                                            Ok(()) => {
                                                info!("DAVE: External sender configurado com sucesso!");
                                                match s.create_key_package() {
                                                    Ok(kp) => { info!("DAVE: KeyPackage gerado ({} bytes)", kp.len()); Some(kp) }
                                                    Err(e) => { warn!("DAVE: Falha ao gerar KeyPackage: {:?}", e); None }
                                                }
                                            }
                                            Err(e) => { warn!("DAVE: Falha ao configurar external sender: {:?}", e); None }
                                        }
                                    } else { None }
                                };
                                if let Some(kp) = key_pkg_bytes {
                                    // Send key package as binary (opcode 26 = dave_mls_key_package)
                                    let mut pkt = vec![26u8];
                                    pkt.extend_from_slice(&kp);
                                    let mut w = write_arc.lock().await;
                                    let _ = w.send(Message::Binary(pkt.into())).await;
                                    info!("DAVE: KeyPackage (op=26) enviado Ã  Voice Gateway!");
                                }
                            }
                            27 => {
                                // dave_mls_proposals (27): MLS proposals [op_type u8][VLBytes proposals]
                                let op_type = match data.get(1).copied().unwrap_or(0) {
                                    0 => ProposalsOperationType::APPEND,
                                    _ => ProposalsOperationType::REVOKE,
                                };
                                let proposals_data = if data.len() > 2 { &data[2..] } else { &[][..] };
                                info!("DAVE: Proposals (op=27) recebido, type={:?}", op_type);
                                let commit_bytes = {
                                    let mut sess = dave_session.lock().unwrap();
                                    if let Some(ref mut s) = *sess {
                                        match s.process_proposals(op_type, proposals_data, None) {
                                            Ok(Some(cw)) => {
                                                info!("DAVE: CommitWelcome gerado ({} commit bytes, welcome={})",
                                                    cw.commit.len(), cw.welcome.as_ref().map(|w| w.len()).unwrap_or(0));
                                                let mut out = cw.commit.clone();
                                                if let Some(w) = cw.welcome {
                                                    out.push(1u8); // RFC 9420 optional<Welcome> presence flag = 1
                                                    out.extend_from_slice(&w);
                                                } else {
                                                    out.push(0u8); // RFC 9420 optional<Welcome> presence flag = 0
                                                }
                                                Some(out)
                                            }
                                            Ok(None) => { info!("DAVE: process_proposals OK sem commit"); None }
                                            Err(e) => { warn!("DAVE: Falha ao processar proposals: {:?}", e); None }
                                        }
                                    } else { None }
                                };
                                if let Some(commit_data) = commit_bytes {
                                    // Send commit+welcome as binary (opcode 28 = dave_mls_commit_welcome)
                                    let mut pkt = vec![28u8];
                                    pkt.extend_from_slice(&commit_data);
                                    let mut w = write_arc.lock().await;
                                    let _ = w.send(Message::Binary(pkt.into())).await;
                                    info!("DAVE: CommitWelcome (op=28) enviado Ã  Voice Gateway!");
                                }
                            }
                            29 => {
                                // dave_mls_announce_commit_transition (29): [op: u8 (29)][transition_id: u16 (2 bytes)][commit_bytes]
                                let transition_id = if data.len() >= 3 {
                                    u16::from_be_bytes([data[1], data[2]]) as u64
                                } else { 0 };
                                let commit_payload = if data.len() > 3 { &data[3..] } else { &[][..] };
                                info!("DAVE: AnnounceCommitTransition (op=29) recebido: transition_id={}", transition_id);
                                dave_flow.prepare(transition_id);
                                let commit_ok = {
                                    let mut sess = dave_session.lock().unwrap();
                                    if let Some(ref mut s) = *sess {
                                        match s.process_commit(commit_payload) {
                                            Ok(()) => {
                                                info!("DAVE: Commit processado com sucesso! is_ready={}", s.is_ready());
                                                true
                                            }
                                            Err(e) => {
                                                warn!("DAVE: Falha ao processar commit: {:?}", e);
                                                false
                                            }
                                        }
                                    } else { false }
                                };
                                if !commit_ok {
                                    tokio::spawn(dave_start_over(
                                        Arc::clone(&dave_flow), Arc::clone(&dave_session), Arc::clone(&saved_external_sender),
                                        Arc::clone(&write_arc), uid_num, cid_num, protocol_version_now(&dave_session),
                                    ));
                                }
                                if commit_ok {
                                    let ready = serde_json::json!({
                                        "op": 23,
                                        "d": { "transition_id": transition_id }
                                    });
                                    let mut w = write_arc.lock().await;
                                    let _ = w.send(Message::Text(ready.to_string().into())).await;
                                    info!("DAVE: Ready For Transition (op=23) enviado para AnnounceCommit transition_id={}", transition_id);
                                }
                            }
                            30 => {
                                // dave_mls_welcome (30): [op: u8 (30)][transition_id: u16 (2 bytes)][welcome_message]
                                let transition_id = if data.len() >= 3 {
                                    u16::from_be_bytes([data[1], data[2]]) as u64
                                } else { 0 };
                                let welcome_payload = if data.len() > 3 { &data[3..] } else { &[][..] };
                                info!("DAVE: Welcome (op=30) recebido! transition_id={}, payload len={}", transition_id, welcome_payload.len());
                                dave_flow.prepare(transition_id);
                                let is_active = {
                                    let mut sess = dave_session.lock().unwrap();
                                    if let Some(ref mut s) = *sess {
                                        match s.process_welcome(welcome_payload) {
                                            Ok(()) => {
                                                info!("ðŸŽ‰ DAVE: Welcome processado com SUCESSO! SessÃ£o ATIVA! is_ready={}", s.is_ready());
                                                true
                                            }
                                            Err(e) => {
                                                warn!("DAVE: Falha ao processar welcome: {:?}", e);
                                                false
                                            }
                                        }
                                    } else { false }
                                };
                                if !is_active {
                                    tokio::spawn(dave_start_over(
                                        Arc::clone(&dave_flow), Arc::clone(&dave_session), Arc::clone(&saved_external_sender),
                                        Arc::clone(&write_arc), uid_num, cid_num, protocol_version_now(&dave_session),
                                    ));
                                }
                                if is_active {
                                    let ready = serde_json::json!({
                                        "op": 23,
                                        "d": { "transition_id": transition_id }
                                    });
                                    let mut w = write_arc.lock().await;
                                    let _ = w.send(Message::Text(ready.to_string().into())).await;
                                    info!("DAVE: Ready For Transition (op=23) enviado para Welcome transition_id={}", transition_id);
                                }
                            }
                            _ => {
                                info!("DAVE: Opcode binÃ¡rio desconhecido: {}", dave_op);
                            }
                        }
                     }
                    Ok(Message::Close(frame)) => {
                        info!("Voice Gateway conexão fechada pelo servidor: {:?}", frame);
                        if let Some(code) = frame.as_ref().map(|f| u16::from(f.code)).filter(|c| VOICE_CLOSE_FINAL.contains(c)) {
                            // Retrying the same session cannot work; a move to another voice
                            // server arrives as a new session from the main gateway.
                            warn!("🔇 [VOICE GATEWAY] Fechamento definitivo ({}): a sessão {} não será reconectada.", code, my_session_id);
                            final_close = true;
                        }
                        break;
                    }
                    Err(e) => {
                        warn!("Aviso de leitura na Voice Gateway: {:?}", e);
                        break;
                    }
                    _ => {}
                }
            }

            set_is_connected_to_voice(false);
            if CURRENT_VOICE_SESSION_ID.load(Ordering::SeqCst) != my_session_id {
                info!("Sessão de voz {} finalizada pelo usuário.", my_session_id);
                clear_voice_participants();
                break;
            }
            if final_close {
                clear_voice_participants();
                break;
            }
            warn!("⚠️ [VOICE GATEWAY] Conexão com Voice Gateway temporariamente perdida. Reconectando em 1.5s...");
            crate::audio::call_sounds::on_voice_connection_lost();
        }
        Err(e) => {
            if CURRENT_VOICE_SESSION_ID.load(Ordering::SeqCst) != my_session_id {
                break;
            }
            warn!("Falha ao conectar na Voice Gateway: {:?}. Tentando reconectar em 800ms...", e);
            tokio::time::sleep(Duration::from_millis(800)).await;
        }
    }
}
}

/// Voice gateway close codes after which the same session must not reconnect: failed
/// authentication, invalid session, disconnected (kicked, channel deleted, server
/// changed), DAVE required, call terminated, disconnected by moderation.
const VOICE_CLOSE_FINAL: [u16; 6] = [4004, 4006, 4014, 4017, 4021, 4022];

/// Packets in a row that DAVE may fail to decrypt before the client asks the server for
/// a new group (the reference clients use the same tolerance).
const DAVE_FAILURE_TOLERANCE: u32 = 36;
/// At most one request for a new group in this long, whatever keeps failing.
const DAVE_START_OVER_EVERY: std::time::Duration = std::time::Duration::from_secs(10);
/// After going back to DAVE, packets without it still play this long (libdave's value).
const DAVE_PLAINTEXT_GRACE: std::time::Duration = std::time::Duration::from_secs(10);

#[derive(Debug, Clone, Copy, PartialEq)]
enum Plaintext {
    Refused,
    Allowed,
    Until(std::time::Instant),
}

type VoiceSink = futures_util::stream::SplitSink<
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>,
    Message,
>;

/// DAVE bookkeeping shared by the gateway handler and the receiver.
struct DaveFlow {
    /// Transition announced and not executed yet, and whether it drops DAVE (version 0).
    pending: std::sync::Mutex<Option<(u64, bool)>>,
    /// When packets sent without DAVE may be played.
    plaintext: std::sync::Mutex<Plaintext>,
    /// The last transition announced, named when telling the server it failed.
    last_transition: std::sync::atomic::AtomicU64,
    /// Received packets in a row that DAVE could not decrypt.
    failures: std::sync::atomic::AtomicU32,
    /// When this client last asked for a new group.
    last_start_over: std::sync::Mutex<Option<std::time::Instant>>,
}

impl DaveFlow {
    fn new() -> Self {
        Self {
            pending: std::sync::Mutex::new(None),
            plaintext: std::sync::Mutex::new(Plaintext::Refused),
            last_transition: std::sync::atomic::AtomicU64::new(0),
            failures: std::sync::atomic::AtomicU32::new(0),
            last_start_over: std::sync::Mutex::new(None),
        }
    }

    /// A transition inside DAVE (a commit or a welcome).
    fn prepare(&self, transition_id: u64) {
        self.prepare_protocol(transition_id, 1);
    }

    /// A protocol transition (op 21). Going to version 0, members that already switched
    /// send audio without DAVE, and it has to play from now on, as libdave does.
    fn prepare_protocol(&self, transition_id: u64, protocol_version: u64) {
        if transition_id != 0 {
            self.last_transition.store(transition_id, Ordering::Relaxed);
        }
        let to_plain = protocol_version == 0;
        if to_plain {
            *self.plaintext.lock().unwrap() = Plaintext::Allowed;
        }
        *self.pending.lock().unwrap() = Some((transition_id, to_plain));
    }

    /// The announced transition took effect: failures before it do not count. Back to
    /// DAVE after a downgrade, packets without it still play for a few seconds, while
    /// the others switch.
    fn execute(&self, transition_id: u64) {
        let mut pending = self.pending.lock().unwrap();
        let Some((id, to_plain)) = *pending else { return };
        if id != transition_id {
            return;
        }
        *pending = None;
        self.failures.store(0, Ordering::Relaxed);
        let mut plaintext = self.plaintext.lock().unwrap();
        if to_plain {
            *plaintext = Plaintext::Allowed;
        } else if *plaintext == Plaintext::Allowed {
            *plaintext = Plaintext::Until(std::time::Instant::now() + DAVE_PLAINTEXT_GRACE);
        }
    }

    /// A packet without the DAVE marker may be played. Outside a downgrade it is refused:
    /// accepting it would let anyone on the media path replace an encrypted voice.
    fn plaintext_allowed(&self) -> bool {
        match *self.plaintext.lock().unwrap() {
            Plaintext::Refused => false,
            Plaintext::Allowed => true,
            Plaintext::Until(t) => std::time::Instant::now() < t,
        }
    }

    /// One more packet DAVE could not decrypt. True when it is time to start over.
    fn failed_once(&self) -> bool {
        let n = self.failures.fetch_add(1, Ordering::Relaxed) + 1;
        if n <= DAVE_FAILURE_TOLERANCE {
            return false;
        }
        self.failures.store(0, Ordering::Relaxed);
        let mut last = self.last_start_over.lock().unwrap();
        if last.is_some_and(|t| t.elapsed() < DAVE_START_OVER_EVERY) {
            return false;
        }
        *last = Some(std::time::Instant::now());
        true
    }
}

fn protocol_version_now(session: &std::sync::Mutex<Option<DaveSession>>) -> u16 {
    session.lock().unwrap().as_ref().map_or(1, |s| s.protocol_version().get())
}

/// The group cannot be used (a commit or welcome failed, or nothing decrypts any more):
/// tell the server the last transition was invalid (op 31) and join again with a new key
/// package, as the reference clients do. Without this, reception stays silent until
/// someone leaves and rejoins the call.
async fn dave_start_over(
    flow: Arc<DaveFlow>,
    session: Arc<std::sync::Mutex<Option<DaveSession>>>,
    external_sender: Arc<std::sync::Mutex<Option<Vec<u8>>>>,
    write: Arc<Mutex<VoiceSink>>,
    user_id: u64,
    channel_id: u64,
    version: u16,
) {
    *flow.last_start_over.lock().unwrap() = Some(std::time::Instant::now());
    let transition_id = flow.last_transition.load(Ordering::Relaxed);
    let key_package = {
        let mut sess = session.lock().unwrap();
        let Some(s) = sess.as_mut() else { return };
        let _ = s.reset();
        match DaveSession::new(NonZeroU16::new(version).unwrap_or(NonZeroU16::MIN), user_id, channel_id, None) {
            Ok(mut fresh) => {
                if let Some(ext) = external_sender.lock().unwrap().as_ref() {
                    let _ = fresh.set_external_sender(ext);
                }
                let kp = fresh.create_key_package().ok();
                *s = fresh;
                kp
            }
            Err(e) => {
                warn!("DAVE: não foi possível recriar a sessão: {:?}", e);
                None
            }
        }
    };
    flow.failures.store(0, Ordering::Relaxed);
    let invalid = serde_json::json!({ "op": 31, "d": { "transition_id": transition_id } });
    let mut w = write.lock().await;
    let _ = w.send(Message::Text(invalid.to_string().into())).await;
    if let Some(kp) = key_package {
        let mut pkt = vec![26u8];
        pkt.extend_from_slice(&kp);
        let _ = w.send(Message::Binary(pkt.into())).await;
    }
    warn!("DAVE: transição {} inválida avisada (op=31) e novo KeyPackage enviado.", transition_id);
}

/// An output stream whose callback has not run for this long is rebuilt.
const OUTPUT_STALL: std::time::Duration = std::time::Duration::from_secs(2);

/// One output callback: mixes the queued voices (`VoiceMixer`) and writes them in the
/// device's channel layout and sample type. Used by the f32 and the i16 stream alike.
/// With `audible` false the voices are mixed (so the queues keep moving) and silence is
/// written.
fn fill_voice_output<T: Copy + Default>(
    output: &mut [T],
    out_channels: usize,
    out: &mut VoiceOutput,
    queues: &std::sync::Mutex<HashMap<u32, VecDeque<(f32, f32)>>>,
    ssrc_to_userid: &std::sync::Mutex<HashMap<u32, u64>>,
    audible: bool,
    convert: impl Fn(f32) -> T,
) {
    output.fill(T::default());
    let out_channels = out_channels.max(1);
    let frames = output.len() / out_channels;
    let Ok(mut queues) = queues.lock() else { return };
    let active_store = get_active_voice_participants_store();
    let generation = user_audio_settings_generation();
    let VoiceOutput { mixer, settings, aec_ref, first_callback } = out;
    if std::mem::take(first_callback) {
        VoiceMixer::drop_backlog(&mut queues);
    }
    if settings.len() > 512 {
        settings.clear();
    }
    let mixed = mixer.mix(&mut queues, frames, smart_ducking(), |ssrc| {
        let uid = ssrc_to_userid.lock().ok().and_then(|m| m.get(&ssrc).copied())
            .or_else(|| active_store.lock().ok().and_then(|m| m.get(&ssrc).copied()))
            .unwrap_or(ssrc as u64);
        // The settings map is keyed by strings and shared with the UI: it is read again
        // only when a setting changed or the SSRC turned out to be another member.
        if let Some((seen_generation, seen_uid, found)) = settings.get(&ssrc) {
            if *seen_generation == generation && *seen_uid == uid {
                return *found;
            }
        }
        let (muted_uid, vol_uid, prio_uid) = get_user_audio_settings(&uid.to_string());
        let (muted_ssrc, vol_ssrc, prio_ssrc) = get_user_audio_settings(&ssrc.to_string());
        let found = (muted_uid || muted_ssrc, if vol_uid != 1.0 { vol_uid } else { vol_ssrc }, prio_uid.max(prio_ssrc));
        settings.insert(ssrc, (generation, uid, found));
        found
    });
    if mixed.is_empty() || !audible {
        return;
    }
    aec_ref.clear();
    for (f, &(l, r)) in mixed.iter().enumerate() {
        aec_ref.push((l + r) * 0.5);
        if out_channels >= 2 {
            output[f * out_channels] = convert(l);
            output[f * out_channels + 1] = convert(r);
        } else {
            output[f] = convert((l + r) * 0.5);
        }
    }
    crate::audio::dsp::push_speaker_reference(aec_ref);
}

/// What one output stream keeps between its callbacks.
struct VoiceOutput {
    mixer: VoiceMixer,
    /// Per SSRC: the settings generation and member it was looked up for, and
    /// (muted, volume, priority).
    settings: HashMap<u32, (u64, u64, (bool, f32, i32))>,
    aec_ref: Vec<f32>,
    first_callback: bool,
}

impl VoiceOutput {
    fn new(out_rate: u32) -> Self {
        Self { mixer: VoiceMixer::new(out_rate), settings: HashMap::new(), aec_ref: Vec::with_capacity(8192), first_callback: true }
    }
}

#[cfg(test)]
mod voice_rx_output_tests {
    use super::*;

    #[test]
    fn deafened_output_is_silent_and_does_not_let_audio_pile_up() {
        let queues = std::sync::Mutex::new(HashMap::new());
        let members = std::sync::Mutex::new(HashMap::new());
        let mut out = VoiceOutput::new(48000);
        let mut heard = 0usize;
        // A member talks for 3 s; the listener is deafened for the first 2 s.
        for callback in 0..300usize {
            if callback % 2 == 0 {
                let frame: Vec<(f32, f32)> = (0..960).map(|i| { let v = 0.4 * ((callback * 480 + i) as f32 * 0.05).sin(); (v, v) }).collect();
                push_voice_frame(queues.lock().unwrap().entry(0x4000_0001).or_default(), &[], &frame);
            }
            let audible = callback >= 200;
            let mut output = [0.5f32; 960];
            fill_voice_output(&mut output, 2, &mut out, &queues, &members, audible, |s| s);
            if !audible {
                assert!(output.iter().all(|s| *s == 0.0), "callback {callback}");
            } else if output.iter().any(|s| *s != 0.0) {
                heard += 1;
            }
            let waiting = queues.lock().unwrap()[&0x4000_0001].len();
            assert!(waiting <= 2880, "callback {callback}: {waiting} pairs waiting");
        }
        assert!(heard >= 99, "audible again at once: {heard} of 100 callbacks");
    }

    #[test]
    fn a_new_stream_does_not_play_what_piled_up_before_it() {
        let queues = std::sync::Mutex::new(HashMap::new());
        let members = std::sync::Mutex::new(HashMap::new());
        queues.lock().unwrap().insert(0x4000_0002u32, VecDeque::from(vec![(0.25f32, 0.25f32); VOICE_QUEUE_LIMIT]));
        let mut out = VoiceOutput::new(48000);
        let mut output = [0i16; 960];
        fill_voice_output(&mut output, 2, &mut out, &queues, &members, true, |s| (s * 32767.0) as i16);
        assert_eq!(queues.lock().unwrap()[&0x4000_0002].len(), 1920 - 480);
    }
}

/// Wait before looking for an output device again: 0.5 s, doubling up to 5 s.
fn next_no_device_wait(previous: Option<Duration>) -> Duration {
    previous.map_or(Duration::from_millis(500), |d| (d * 2).min(Duration::from_secs(5)))
}

#[cfg(test)]
mod dave_flow_tests {
    use super::*;

    #[test]
    fn an_executed_transition_clears_earlier_failures() {
        let flow = DaveFlow::new();
        flow.prepare(7);
        for _ in 0..10 {
            flow.failed_once();
        }
        flow.execute(8);
        assert_eq!(flow.failures.load(Ordering::Relaxed), 10, "another transition id changes nothing");
        flow.execute(7);
        assert_eq!(flow.failures.load(Ordering::Relaxed), 0);
        assert_eq!(flow.last_transition.load(Ordering::Relaxed), 7);
    }

    #[test]
    fn plaintext_plays_only_from_a_downgrade_until_shortly_after_dave_returns() {
        let flow = DaveFlow::new();
        assert!(!flow.plaintext_allowed(), "never outside a downgrade");
        flow.prepare_protocol(3, 1);
        flow.execute(3);
        assert!(!flow.plaintext_allowed(), "a transition that keeps DAVE allows nothing");

        flow.prepare_protocol(4, 0);
        assert!(flow.plaintext_allowed(), "already while the downgrade is announced");
        flow.execute(4);
        assert!(flow.plaintext_allowed());

        flow.prepare(5);
        assert!(flow.plaintext_allowed(), "still downgraded until the upgrade is executed");
        flow.execute(5);
        assert!(flow.plaintext_allowed(), "a grace period while the others switch back");
        *flow.plaintext.lock().unwrap() = Plaintext::Until(std::time::Instant::now());
        assert!(!flow.plaintext_allowed());
    }

    #[test]
    fn decrypt_failures_start_over_after_the_tolerance_at_most_every_ten_seconds() {
        let flow = DaveFlow::new();
        for _ in 0..DAVE_FAILURE_TOLERANCE {
            assert!(!flow.failed_once());
        }
        assert!(flow.failed_once());
        for _ in 0..=DAVE_FAILURE_TOLERANCE * 3 {
            assert!(!flow.failed_once(), "no second request within ten seconds");
        }
        *flow.last_start_over.lock().unwrap() = Some(std::time::Instant::now() - DAVE_START_OVER_EVERY);
        flow.failures.store(0, Ordering::Relaxed);
        for _ in 0..DAVE_FAILURE_TOLERANCE {
            flow.failed_once();
        }
        assert!(flow.failed_once());
    }
}

#[cfg(test)]
mod output_device_wait_tests {
    use super::*;

    #[test]
    fn output_device_retry_backs_off_to_five_seconds() {
        let mut wait = None;
        let mut seen = Vec::new();
        for _ in 0..6 {
            let next = next_no_device_wait(wait);
            seen.push(next.as_millis());
            wait = Some(next);
        }
        assert_eq!(seen, vec![500, 1000, 2000, 4000, 5000, 5000]);
    }
}


#[cfg(test)]
mod voice_playout_tests {
    use super::*;
    use crate::gateway::voice_rx::decoded_voice_pairs;

    #[test]
    fn quiet_speech_keeps_its_samples_in_mono_and_stereo() {
        assert_eq!(decoded_voice_pairs(&[0.001, -0.002], 1), vec![(0.001, 0.001), (-0.002, -0.002)]);
        assert_eq!(decoded_voice_pairs(&[0.001, -0.002, 2.0, -2.0], 2), vec![(0.001, -0.002), (1.0, -1.0)]);
    }

    #[test]
    fn backlog_keeps_latest_audio_and_orders_concealment_before_new_samples() {
        let mut q: VecDeque<_> = (0..VOICE_QUEUE_LIMIT).map(|i| (i as f32, i as f32)).collect();
        push_voice_frame(&mut q, &vec![(-1.0, -1.0); 960], &vec![(9.0, 9.0); 960]);
        assert_eq!(q.len(), VOICE_QUEUE_LIMIT);
        assert_eq!(q.front(), Some(&(1920.0, 1920.0)));
        assert_eq!(q[VOICE_QUEUE_LIMIT - 1920], (-1.0, -1.0));
        assert_eq!(q.back(), Some(&(9.0, 9.0)));
    }

    #[test]
    fn even_an_oversized_input_never_exceeds_the_voice_backlog_limit() {
        let mut q = VecDeque::from([(99.0, 99.0)]);
        let decoded: Vec<_> = (0..VOICE_QUEUE_LIMIT + 2400).map(|i| (i as f32, i as f32)).collect();
        push_voice_frame(&mut q, &vec![(-1.0, -1.0); 1920], &decoded);
        assert_eq!(q.len(), VOICE_QUEUE_LIMIT);
        assert_eq!(q.front(), Some(&(2400.0, 2400.0)));
        assert_eq!(q.back(), Some(&((VOICE_QUEUE_LIMIT + 2399) as f32, (VOICE_QUEUE_LIMIT + 2399) as f32)));
    }
}
