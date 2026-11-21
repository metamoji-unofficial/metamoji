//! The relay socket.
//!
//! A raw TLS connection to the address `cosmos/LoginRoom` hands back, speaking
//! the line protocol in `wire.rs`. Not WebSocket, not HTTP — the relay predates
//! both in this app's history and `NsCollaboSocket` implements the framing by
//! hand.
//!
//! Two details from `docs/typespec/classroom/collabo-socket-protocol.md` §2
//! that shape everything here:
//!
//! * **There is no handshake.** Connecting establishes TLS and nothing else;
//!   the session begins because the client sends `LoginRoom` as an ordinary
//!   command.
//! * **The client never pings.** It answers the server's `Ping`, and it runs a
//!   watchdog that drops the connection after 30 seconds of silence. So a dead
//!   peer is detected by absence, not by probing.
//!
//! Reconnection is deliberately *not* here — `NsCollaboSocket` has none either,
//! and putting a retry loop inside the transport hides the state the caller
//! needs to rebuild (which booths were attached, at which sequence numbers).

use std::sync::Arc;
use std::time::Duration;

use serde::Serialize;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio_rustls::rustls::pki_types::ServerName;
use tokio_rustls::rustls::{ClientConfig, RootCertStore};
use tokio_rustls::TlsConnector;

use super::wire::{Decoder, Frame, PacketCounter};
use crate::error::{AppError, AppResult};

/// §2: no traffic for this long means the peer is gone.
const WATCHDOG: Duration = Duration::from_secs(30);

/// What the room does to us. Named for the reader, not for the wire.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum CollaboEvent {
    Connected,
    /// `LoginRoomResult`. `ok == false` carries the server's `msg`.
    LoggedIn {
        ok: bool,
        message: Option<String>,
        room_type: Option<String>,
        user_id: Option<String>,
        roles: Vec<String>,
    },
    /// A booth has data we have not seen; the caller decides whether to fetch.
    BoothUpdated {
        booth_id: String,
    },
    /// Someone joined, left, or had a property changed.
    RoomUpdated {
        key: String,
        value: String,
        user_id: Option<String>,
    },
    ModeChanged {
        key: String,
        enabled: bool,
    },
    /// Self-scoped: the server sends no `uid`, so this is always about us.
    RoleChanged {
        key: String,
        enabled: bool,
    },
    /// A teacher's broadcast message. Both fields arrive base64-encoded.
    Message {
        title: Option<String>,
        body: String,
    },
    /// Room settings, as the base64 JSON the server sent.
    SettingChanged {
        setting: String,
    },
    /// An edit from another participant. `payload` is a serialised model.
    Direction {
        booth_id: String,
        sequence: i64,
        user_id: Option<String>,
        /// True when the relay is echoing our own post back to us.
        own_echo: bool,
        #[serde(skip)]
        payload: Vec<u8>,
    },
    /// Acknowledgement of one of our `PostData` frames.
    PostAck {
        booth_id: String,
        packet_no: String,
        ok: bool,
    },
    /// Answer to `AttachBooth`. A refusal is why a booth stays silent, so it
    /// is worth having rather than inferring from the absence of edits.
    BoothAttached {
        booth_id: String,
        ok: bool,
    },
    /// The room ended. The original logs out immediately on this.
    Finished,
    Disconnected {
        reason: String,
    },
}

/// Commands the caller can send once connected.
#[derive(Debug, Clone)]
pub enum Command {
    Login {
        room_id: String,
        /// The id the collabo service issued this install (`cosmos/CreateUniqueID`).
        /// `did` is *not* the drive id, despite the name: `NsCollaboManager$8`
        /// passes `NsCollaboDeviceInfo.getDeviceId()`. Sending a drive id here
        /// is accepted, and lands you in the room as a `visitor` who cannot
        /// attach a single booth.
        device_id: String,
        session_id: String,
        nickname: String,
    },
    Logout {
        room_id: String,
    },
    AttachBooth {
        booth_id: String,
        last_sequence: i64,
    },
    DetachBooth {
        booth_id: String,
    },
    /// An edit. Always binary in practice — §6.1.
    PostData {
        booth_id: String,
        payload: Vec<u8>,
        /// `loop`: ask the relay to echo it back to us.
        send_back: bool,
        /// `save`: ask the relay to keep it for late joiners.
        save: bool,
        rip_off_size: String,
    },
    /// Asks to be told when a booth gains data, rather than being sent it.
    ///
    /// The relay does not push a user their own posts — another device signed
    /// in as the same person hears nothing about them. This is how it finds
    /// out: `bidseq` lists `{boothId}={lastSequence}` and the relay answers
    /// with `BoothUpdated` when a booth moves past the sequence given.
    NotifyBoothUpdated {
        marks: Vec<(String, i64)>,
    },
    /// Only ever sent in reply to the server's `Ping` (§4).
    PingResult,
    Disconnect,
}

impl Command {
    fn to_frame(&self) -> Frame {
        match self {
            Command::Login {
                room_id,
                device_id,
                session_id,
                nickname,
            } => {
                use base64::Engine as _;
                // Parameters are space-separated, so a name with a space in it
                // would end the frame early. The original base64s it.
                let name = base64::engine::general_purpose::STANDARD.encode(nickname.as_bytes());
                Frame::new("LoginRoom")
                    .param("rid", room_id)
                    .param("did", device_id)
                    .param("sid", session_id)
                    .param("name", name.trim())
            }
            Command::Logout { room_id } => Frame::new("LogoutRoom").param("rid", room_id),
            Command::AttachBooth {
                booth_id,
                last_sequence,
            } => Frame::new("AttachBooth")
                .param("bid", booth_id)
                .param("last", last_sequence),
            Command::DetachBooth { booth_id } => Frame::new("DetachBooth").param("bid", booth_id),
            Command::PostData {
                booth_id,
                payload,
                send_back,
                save,
                rip_off_size,
            } => Frame::new("PostData")
                .booth(booth_id)
                .param("loop", i32::from(*send_back))
                .param("save", i32::from(*save))
                .param("rsize", rip_off_size)
                .param("binary", payload.len())
                .payload(payload.clone()),
            Command::NotifyBoothUpdated { marks } => Frame::new("AddNotifyBoothUpdated").param(
                "bidseq",
                marks
                    .iter()
                    .map(|(booth, sequence)| format!("{booth}={sequence}"))
                    .collect::<Vec<_>>()
                    .join(","),
            ),
            Command::PingResult => Frame::new("PingResult"),
            // Never framed: the writer task breaks out before encoding it.
            Command::Disconnect => Frame::new("Disconnect"),
        }
    }
}

/// Translates one received frame into an event, or `None` for the pushes the
/// original recognises and then ignores (`OpenConnection`, `PropertyChanged`,
/// and every `*Result` it documents as a no-op).
pub fn event_for(frame: &Frame) -> Option<CollaboEvent> {
    let enabled = |frame: &Frame| frame.get("value") == Some("enable");

    match frame.command.as_str() {
        "LoginRoomResult" => Some(CollaboEvent::LoggedIn {
            ok: succeeded(frame),
            message: frame.get("msg").map(str::to_string),
            room_type: frame.get("rtype").map(str::to_string),
            user_id: frame.get("uid").map(str::to_string),
            roles: frame
                .get("role")
                .map(|r| {
                    r.split(',')
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                        .map(str::to_string)
                        .collect()
                })
                .unwrap_or_default(),
        }),
        "BoothUpdated" => Some(CollaboEvent::BoothUpdated {
            booth_id: frame.get("bid")?.to_string(),
        }),
        "RoomUpdated" => Some(CollaboEvent::RoomUpdated {
            key: frame.get("key").unwrap_or_default().to_string(),
            value: frame.get("value").unwrap_or_default().to_string(),
            user_id: frame.get("uid").map(str::to_string),
        }),
        "ModeChanged" => Some(CollaboEvent::ModeChanged {
            key: frame.get("key")?.to_string(),
            enabled: enabled(frame),
        }),
        // Role changes do not come through `RoomUpdated` — §5 notes there is no
        // `value:"role"` branch — they arrive as their own event.
        "RoleChanged" => Some(CollaboEvent::RoleChanged {
            key: frame.get("key")?.to_string(),
            enabled: enabled(frame),
        }),
        "PostMessage" => Some(CollaboEvent::Message {
            title: frame.get("title").map(str::to_string),
            body: frame.get("message")?.to_string(),
        }),
        "SettingChanged" => Some(CollaboEvent::SettingChanged {
            setting: frame.get("setting")?.to_string(),
        }),
        "PostData" => Some(CollaboEvent::Direction {
            // The booth is the frame's channel, not a parameter.
            booth_id: frame.booth_id.clone(),
            sequence: frame.get_i64("seq").unwrap_or(0),
            user_id: frame.get("uid").map(str::to_string),
            own_echo: frame.get_i64("self") == Some(1),
            payload: direction_payload(frame),
        }),
        "AttachBoothResult" => Some(CollaboEvent::BoothAttached {
            booth_id: frame
                .get("bid")
                .map(str::to_string)
                .unwrap_or_else(|| frame.booth_id.clone()),
            ok: succeeded(frame),
        }),
        "PostDataResult" => Some(CollaboEvent::PostAck {
            booth_id: frame.booth_id.clone(),
            packet_no: frame.packet_no.clone(),
            ok: succeeded(frame),
        }),
        "Finish" => Some(CollaboEvent::Finished),
        _ => None,
    }
}

/// Whether a `*Result` frame reports success.
///
/// `status` is an error *code*, not a boolean: the relay answers `status:0` to
/// everything it accepted. Reading it as `"true"` marks every successful login
/// and every successful booth attach as a failure, which looks exactly like
/// being refused.
fn succeeded(frame: &Frame) -> bool {
    match frame.get("status") {
        Some("0") | Some("true") => true,
        Some(_) => false,
        None => true,
    }
}

/// Prints every frame when `METAMOJI_COLLABO_TRACE` is set.
///
/// The relay says why it refused something in parameters this module does not
/// model, and there is no other way to see them: the connection is TLS to a
/// port that speaks a protocol no proxy understands.
fn trace(frame: &Frame) {
    if std::env::var_os("METAMOJI_COLLABO_TRACE").is_none() {
        return;
    }
    eprintln!(
        "<- [{}] {} {:?}{}",
        frame.booth_id,
        frame.command,
        frame.params,
        if frame.payload.is_empty() {
            String::new()
        } else {
            format!(" +{} bytes", frame.payload.len())
        }
    );
}

/// The bytes of a received edit.
///
/// `binaryData` and `data` are exclusive (§5): the binary path puts the payload
/// after the header, the text path base64-encodes it into a parameter. Reading
/// only the first drops every edit that arrived the other way.
fn direction_payload(frame: &Frame) -> Vec<u8> {
    if !frame.payload.is_empty() {
        return frame.payload.clone();
    }
    use base64::Engine as _;
    frame
        .get("data")
        .and_then(|text| base64::engine::general_purpose::STANDARD.decode(text).ok())
        .unwrap_or_default()
}

/// A live connection. Dropping the sender closes it.
pub struct Connection {
    pub commands: mpsc::Sender<Command>,
}

fn tls_config() -> Arc<ClientConfig> {
    let roots = RootCertStore {
        roots: webpki_roots::TLS_SERVER_ROOTS.to_vec(),
    };
    Arc::new(
        ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth(),
    )
}

/// Connects and runs until the peer goes away or the caller drops the sender.
///
/// `on_event` is called from the reader task, so it must not block.
pub async fn connect<F>(host: &str, port: u16, on_event: F) -> AppResult<Connection>
where
    F: Fn(CollaboEvent) + Send + Sync + 'static,
{
    let tcp = TcpStream::connect((host, port)).await.map_err(|e| {
        AppError::other(format!("教室サーバーに接続できません ({host}:{port}): {e}"))
    })?;
    // Nagle would hold a stroke back waiting for company; these frames are
    // small and latency is the whole point.
    let _ = tcp.set_nodelay(true);

    let name = ServerName::try_from(host.to_string())
        .map_err(|_| AppError::other(format!("サーバー名が不正です: {host}")))?;
    let stream = TlsConnector::from(tls_config())
        .connect(name, tcp)
        .await
        .map_err(|e| AppError::other(format!("TLS ハンドシェイクに失敗しました: {e}")))?;

    let (mut reader, mut writer) = tokio::io::split(stream);
    let (tx, mut rx) = mpsc::channel::<Command>(64);
    let on_event = Arc::new(on_event);

    // Writer: owns the packet counter, so numbering stays monotonic even when
    // several callers post at once.
    tokio::spawn(async move {
        let mut counter = PacketCounter::default();
        while let Some(command) = rx.recv().await {
            if matches!(command, Command::Disconnect) {
                break;
            }
            let bytes = command.to_frame().encode(&counter.next_number());
            if writer.write_all(&bytes).await.is_err() {
                break;
            }
            let _ = writer.flush().await;
        }
        let _ = writer.shutdown().await;
    });

    // Reader: decodes frames, answers `Ping`, and enforces the watchdog.
    let events = Arc::clone(&on_event);
    let ping_tx = tx.clone();
    tokio::spawn(async move {
        let mut decoder = Decoder::default();
        let mut buffer = [0u8; 8192];
        events(CollaboEvent::Connected);

        let reason = loop {
            let read = tokio::time::timeout(WATCHDOG, reader.read(&mut buffer)).await;
            let count = match read {
                // §2: silence, not a failed probe, is how a dead peer is found.
                Err(_) => break "サーバーからの応答が 30 秒ありません".to_string(),
                Ok(Err(e)) => break format!("受信に失敗しました: {e}"),
                Ok(Ok(0)) => break "サーバーが接続を閉じました".to_string(),
                Ok(Ok(n)) => n,
            };

            decoder.push(&buffer[..count]);
            let mut over_limit = None;
            loop {
                match decoder.next_frame() {
                    Ok(Some(frame)) => {
                        trace(&frame);
                        if frame.command == "Ping" {
                            // Answered here rather than surfaced: nothing
                            // above this layer can do anything useful with a
                            // ping.
                            let _ = ping_tx.try_send(Command::PingResult);
                            continue;
                        }
                        if let Some(event) = event_for(&frame) {
                            events(event);
                        }
                    }
                    Ok(None) => break,
                    Err(e) => {
                        over_limit = Some(e.to_string());
                        break;
                    }
                }
            }
            if let Some(reason) = over_limit {
                break reason;
            }
        };

        events(CollaboEvent::Disconnected { reason });
    });

    Ok(Connection { commands: tx })
}

#[path = "socket/tests.rs"]
#[cfg(test)]
mod tests;
