//! The stream's media socket, and Discord's IP discovery: the client sends 74 bytes naming
//! its SSRC to the media server, which answers with the address and port it saw them come
//! from. Select Protocol then tells the server that address.

use std::io;
use std::net::SocketAddr;
use std::time::Duration;

use tokio::net::UdpSocket;

pub const DISCOVERY_LEN: usize = 74;
/// Room for a few keyframes at once, as the web share's socket has.
const SEND_BUFFER: usize = 4 << 20;
const RECV_BUFFER: usize = 1 << 20;

/// A socket for one stream connection, sending to and hearing from `server` only.
pub async fn open(server: SocketAddr) -> io::Result<UdpSocket> {
    let bind: SocketAddr = if server.is_ipv4() { ([0, 0, 0, 0], 0).into() } else { ([0u16; 8], 0).into() };
    let socket = std::net::UdpSocket::bind(bind)?;
    let raw = socket2::SockRef::from(&socket);
    let _ = raw.set_send_buffer_size(SEND_BUFFER);
    let _ = raw.set_recv_buffer_size(RECV_BUFFER);
    socket.set_nonblocking(true)?;
    let socket = UdpSocket::from_std(socket)?;
    socket.connect(server).await?;
    Ok(socket)
}

/// `00 01 | 00 46 | ssrc | 66 zero bytes`: type 1 (request), length 70, the SSRC, and room
/// for the address and port of the answer.
pub fn discovery_request(ssrc: u32) -> [u8; DISCOVERY_LEN] {
    let mut packet = [0u8; DISCOVERY_LEN];
    packet[0..2].copy_from_slice(&1u16.to_be_bytes());
    packet[2..4].copy_from_slice(&70u16.to_be_bytes());
    packet[4..8].copy_from_slice(&ssrc.to_be_bytes());
    packet
}

/// The address and port in the server's answer: a NUL-terminated string from byte 8, and
/// the last two bytes, big endian.
pub fn parse_discovery_response(packet: &[u8]) -> Option<(String, u16)> {
    if packet.len() < 70 {
        return None;
    }
    let address = &packet[8..packet.len() - 2];
    let address = &address[..address.iter().position(|b| *b == 0).unwrap_or(address.len())];
    let address = std::str::from_utf8(address).ok()?.trim();
    let port = u16::from_be_bytes([packet[packet.len() - 2], packet[packet.len() - 1]]);
    // (An address, not anything else that would end up in Select Protocol.)
    (address.parse::<std::net::IpAddr>().is_ok() && port != 0).then(|| (address.to_string(), port))
}

/// Asks the server which address it sees this socket at: `tries` requests, each waiting
/// `wait` for the answer. `None` when none came.
pub async fn discover(socket: &UdpSocket, ssrc: u32, tries: u32, wait: Duration) -> Option<(String, u16)> {
    let request = discovery_request(ssrc);
    let mut answer = [0u8; 128];
    for _ in 0..tries {
        if socket.send(&request).await.is_err() {
            tokio::time::sleep(wait).await;
            continue;
        }
        let deadline = tokio::time::Instant::now() + wait;
        // (Anything else that arrives meanwhile is not the answer: keep waiting for it.)
        loop {
            match tokio::time::timeout_at(deadline, socket.recv(&mut answer)).await {
                Ok(Ok(len)) => {
                    if let Some(found) = parse_discovery_response(&answer[..len]) {
                        return Some(found);
                    }
                }
                // (A refused port comes back as an error at once: still wait before asking again.)
                Ok(Err(_)) => {
                    tokio::time::sleep_until(deadline).await;
                    break;
                }
                Err(_) => break,
            }
        }
    }
    None
}
