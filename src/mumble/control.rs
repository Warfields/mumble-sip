use std::collections::HashMap;
use std::net::SocketAddr;
use std::pin::Pin;

use bytes::Bytes;
use futures::{SinkExt, StreamExt};
use mumble_protocol_2x::Serverbound;
use mumble_protocol_2x::control::msgs;
use mumble_protocol_2x::control::{ClientControlCodec, ControlPacket};
use mumble_protocol_2x::crypt::ClientCryptState;
use mumble_protocol_2x::voice::{VoicePacket, VoicePacketPayload};
use openssl::ssl::{SslConnector, SslMethod, SslVerifyMode};
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio::time::{self, Duration};
use tokio_openssl::SslStream;
use tokio_util::codec::Decoder;
use tracing::{debug, error, info, warn};

/// Events emitted by the Mumble control connection.
#[derive(Debug)]
pub enum MumbleEvent {
    Connected {
        session_id: u32,
    },
    Disconnected,
    AudioReceived {
        session_id: u32,
        opus_data: Bytes,
        seq_num: u64,
    },
    UserChangedChannel {
        session_id: u32,
        channel_id: u32,
    },
    ChannelState {
        channel_id: u32,
        name: String,
    },
    UserDisconnected {
        session_id: u32,
    },
    TextMessageReceived {
        sender_name: Option<String>,
        message: String,
    },
}

/// Configuration for connecting to a Mumble server.
#[derive(Clone, Debug)]
pub struct MumbleConfig {
    pub host: String,
    pub port: u16,
    pub username: String,
    pub password: String,
    pub channel: String,
    pub accept_invalid_cert: bool,
}

/// A Mumble client connection. Each SIP call gets its own instance.
pub struct MumbleClient {
    outgoing_tx: mpsc::UnboundedSender<ControlPacket<Serverbound>>,
    event_rx: mpsc::UnboundedReceiver<MumbleEvent>,
    session_id: u32,
    /// Channel ID we joined at connect time (0 = root/default).
    channel_id: u32,
    /// Highest channel ID known at connect time.
    max_channel_id: u32,
    /// Channel names known from ChannelState messages.
    channel_names: HashMap<u32, String>,
    recv_task: tokio::task::JoinHandle<()>,
    send_task: tokio::task::JoinHandle<()>,
}

impl MumbleClient {
    /// Connect to a Mumble server, authenticate, and wait for ServerSync.
    pub async fn connect(config: &MumbleConfig) -> anyhow::Result<Self> {
        let addr: SocketAddr = tokio::net::lookup_host((&*config.host, config.port))
            .await?
            .next()
            .ok_or_else(|| anyhow::anyhow!("Failed to resolve {}:{}", config.host, config.port))?;

        // TCP connect
        let tcp_stream = TcpStream::connect(addr).await?;
        debug!("TCP connected to {}", addr);

        // TLS setup
        let mut ssl_builder = SslConnector::builder(SslMethod::tls())?;
        if config.accept_invalid_cert {
            ssl_builder.set_verify(SslVerifyMode::NONE);
        }
        let ssl_connector = ssl_builder.build();
        let ssl = ssl_connector.configure()?.into_ssl(&config.host)?;
        let mut tls_stream = SslStream::new(ssl, tcp_stream)?;
        Pin::new(&mut tls_stream).connect().await?;
        debug!("TLS connected");

        // Frame with Mumble's control codec
        let (mut sink, mut stream) = ClientControlCodec::new().framed(tls_stream).split();

        // Send Authenticate
        let mut auth = msgs::Authenticate::new();
        auth.set_username(config.username.clone());
        if !config.password.is_empty() {
            auth.set_password(config.password.clone());
        }
        auth.set_opus(true);
        sink.send(auth.into()).await?;
        debug!("Sent Authenticate");

        // Wait for CryptSetup and ServerSync during handshake
        let mut _crypt_state: Option<ClientCryptState> = None;
        let mut session_id: Option<u32> = None;
        let mut target_channel_id: Option<u32> = None;
        let mut max_channel_id: u32 = 0;
        let mut channel_names: HashMap<u32, String> = HashMap::new();
        let mut user_names: HashMap<u32, String> = HashMap::new();

        while let Some(packet) = stream.next().await {
            match packet? {
                ControlPacket::CryptSetup(msg) => {
                    _crypt_state = Some(ClientCryptState::new_from(
                        msg.key()
                            .try_into()
                            .map_err(|_| anyhow::anyhow!("Invalid key size"))?,
                        msg.client_nonce()
                            .try_into()
                            .map_err(|_| anyhow::anyhow!("Invalid client_nonce size"))?,
                        msg.server_nonce()
                            .try_into()
                            .map_err(|_| anyhow::anyhow!("Invalid server_nonce size"))?,
                    ));
                    debug!("Received CryptSetup");
                }
                ControlPacket::ChannelState(msg) => {
                    let ch_id = msg.channel_id();
                    channel_names.insert(ch_id, msg.name().to_string());
                    max_channel_id = max_channel_id.max(ch_id);
                    if !config.channel.is_empty() && msg.name() == config.channel {
                        target_channel_id = Some(ch_id);
                        debug!("Found target channel '{}' (id={})", config.channel, ch_id);
                    }
                }
                ControlPacket::UserState(msg) => {
                    Self::update_user_name_map(&msg, &mut user_names);
                }
                ControlPacket::ServerSync(msg) => {
                    session_id = Some(msg.session());
                    info!("Logged in to Mumble (session_id={})", msg.session());
                    break;
                }
                ControlPacket::Reject(msg) => {
                    return Err(anyhow::anyhow!("Login rejected: {:?}", msg));
                }
                other => {
                    debug!("Handshake received: {}", other.name());
                }
            }
        }

        let session_id =
            session_id.ok_or_else(|| anyhow::anyhow!("Connection closed before ServerSync"))?;

        // Join the target channel if found
        if let Some(ch_id) = target_channel_id {
            let mut user_state = msgs::UserState::new();
            user_state.set_channel_id(ch_id);
            sink.send(user_state.into()).await?;
            info!("Joined channel '{}' (id={})", config.channel, ch_id);
        }

        // Set up event and outgoing channels
        let (event_tx, event_rx) = mpsc::unbounded_channel();
        let (outgoing_tx, mut outgoing_rx) =
            mpsc::unbounded_channel::<ControlPacket<Serverbound>>();

        // Spawn receive loop
        let event_tx_recv = event_tx.clone();
        let recv_task = tokio::spawn(async move {
            let mut user_names = user_names;
            while let Some(packet) = stream.next().await {
                match packet {
                    Ok(packet) => Self::handle_packet(packet, &event_tx_recv, &mut user_names),
                    Err(e) => {
                        error!("Error receiving from Mumble: {}", e);
                        break;
                    }
                }
            }
            warn!("Mumble connection closed");
            let _ = event_tx_recv.send(MumbleEvent::Disconnected);
        });

        // Spawn send loop: drains outgoing channel + periodic pings
        let send_task = tokio::spawn(async move {
            let mut ping_interval = time::interval(Duration::from_secs(15));

            loop {
                tokio::select! {
                    _ = ping_interval.tick() => {
                        let ping = msgs::Ping::new();
                        if let Err(e) = sink.send(ping.into()).await {
                            error!("Failed to send ping: {}", e);
                            break;
                        }
                    }
                    msg = outgoing_rx.recv() => {
                        match msg {
                            Some(packet) => {
                                if let Err(e) = sink.send(packet).await {
                                    error!("Failed to send to Mumble: {}", e);
                                    break;
                                }
                            }
                            None => break, // Channel closed
                        }
                    }
                }
            }
        });

        Ok(MumbleClient {
            outgoing_tx,
            event_rx,
            session_id,
            channel_id: target_channel_id.unwrap_or(0),
            max_channel_id,
            channel_names,
            recv_task,
            send_task,
        })
    }

    fn handle_packet(
        packet: ControlPacket<mumble_protocol_2x::Clientbound>,
        event_tx: &mpsc::UnboundedSender<MumbleEvent>,
        user_names: &mut HashMap<u32, String>,
    ) {
        match packet {
            ControlPacket::UDPTunnel(voice_packet) => {
                if let VoicePacket::Audio {
                    session_id,
                    seq_num,
                    payload,
                    ..
                } = *voice_packet
                {
                    if let VoicePacketPayload::Opus(data, _eot) = payload {
                        let _ = event_tx.send(MumbleEvent::AudioReceived {
                            session_id,
                            opus_data: data,
                            seq_num,
                        });
                    }
                }
            }
            ControlPacket::UserState(msg) => {
                debug!("User state: session={} name={}", msg.session(), msg.name());
                Self::update_user_name_map(&msg, user_names);
                if msg.has_channel_id() {
                    let _ = event_tx.send(MumbleEvent::UserChangedChannel {
                        session_id: msg.session(),
                        channel_id: msg.channel_id(),
                    });
                }
            }
            ControlPacket::ChannelState(msg) => {
                let _ = event_tx.send(MumbleEvent::ChannelState {
                    channel_id: msg.channel_id(),
                    name: msg.name().to_string(),
                });
            }
            ControlPacket::UserRemove(msg) => {
                debug!("User removed: session={}", msg.session());
                Self::remove_user_from_map(msg.session(), user_names);
                let _ = event_tx.send(MumbleEvent::UserDisconnected {
                    session_id: msg.session(),
                });
            }
            ControlPacket::TextMessage(msg) => {
                debug!("Text message: {}", msg.message());
                let _ = event_tx.send(MumbleEvent::TextMessageReceived {
                    sender_name: Self::resolve_text_message_sender_name(&msg, user_names),
                    message: msg.message().to_string(),
                });
            }
            ControlPacket::Ping(_) => {}
            _ => {}
        }
    }

    fn update_user_name_map(msg: &msgs::UserState, user_names: &mut HashMap<u32, String>) {
        if !msg.has_session() || !msg.has_name() {
            return;
        }
        let session_id = msg.session();
        let name = msg.name().trim();
        if name.is_empty() {
            user_names.remove(&session_id);
        } else {
            user_names.insert(session_id, name.to_string());
        }
    }

    fn remove_user_from_map(session_id: u32, user_names: &mut HashMap<u32, String>) {
        user_names.remove(&session_id);
    }

    fn resolve_text_message_sender_name(
        msg: &msgs::TextMessage,
        user_names: &HashMap<u32, String>,
    ) -> Option<String> {
        if !msg.has_actor() {
            return None;
        }
        user_names.get(&msg.actor()).cloned()
    }

    /// Receive the next event from the Mumble connection.
    pub async fn recv_event(&mut self) -> Option<MumbleEvent> {
        self.event_rx.recv().await
    }

    /// Join a channel by ID. Kept as a separate method for future channel navigation.
    pub fn join_channel(&self, channel_id: u32) -> anyhow::Result<()> {
        let mut msg = msgs::UserState::new();
        msg.set_channel_id(channel_id);
        self.send_control(msg.into())
    }

    /// Send an Opus voice packet via UDPTunnel (TCP-tunneled voice).
    pub fn send_voice(
        &self,
        seq_num: u64,
        opus_data: Bytes,
        end_of_transmission: bool,
    ) -> anyhow::Result<()> {
        let voice = VoicePacket::Audio {
            _dst: std::marker::PhantomData,
            target: 0,
            session_id: (),
            seq_num,
            payload: VoicePacketPayload::Opus(opus_data, end_of_transmission),
            position_info: None,
        };
        self.send_control(ControlPacket::UDPTunnel(Box::new(voice)))
    }

    fn send_control(&self, packet: ControlPacket<Serverbound>) -> anyhow::Result<()> {
        self.outgoing_tx
            .send(packet)
            .map_err(|_| anyhow::anyhow!("Mumble connection closed"))
    }

    pub fn session_id(&self) -> u32 {
        self.session_id
    }

    /// Returns the channel ID joined at connect time (0 = root/default).
    pub fn channel_id(&self) -> u32 {
        self.channel_id
    }

    /// Returns the highest channel ID known at connect time.
    pub fn max_channel_id(&self) -> u32 {
        self.max_channel_id
    }

    /// Returns the connect-time channel map (channel_id -> channel name).
    pub fn channel_names(&self) -> HashMap<u32, String> {
        self.channel_names.clone()
    }

    /// Get a clonable sender handle for sending voice/control from other tasks.
    pub fn sender(&self) -> MumbleSender {
        MumbleSender {
            outgoing_tx: self.outgoing_tx.clone(),
        }
    }
}

impl Drop for MumbleClient {
    fn drop(&mut self) {
        self.recv_task.abort();
        self.send_task.abort();
    }
}

/// A clonable handle for sending packets to Mumble.
/// Can be shared across tasks without owning the MumbleClient.
#[derive(Clone)]
pub struct MumbleSender {
    outgoing_tx: mpsc::UnboundedSender<ControlPacket<Serverbound>>,
}

impl MumbleSender {
    pub fn join_channel(&self, channel_id: u32) -> anyhow::Result<()> {
        let mut msg = msgs::UserState::new();
        msg.set_channel_id(channel_id);
        self.outgoing_tx
            .send(msg.into())
            .map_err(|_| anyhow::anyhow!("Mumble connection closed"))
    }

    pub fn send_voice(
        &self,
        seq_num: u64,
        opus_data: Bytes,
        end_of_transmission: bool,
    ) -> anyhow::Result<()> {
        let voice = VoicePacket::Audio {
            _dst: std::marker::PhantomData,
            target: 0,
            session_id: (),
            seq_num,
            payload: VoicePacketPayload::Opus(opus_data, end_of_transmission),
            position_info: None,
        };
        self.outgoing_tx
            .send(ControlPacket::UDPTunnel(Box::new(voice)))
            .map_err(|_| anyhow::anyhow!("Mumble connection closed"))
    }
}

#[cfg(test)]
mod tests {
    use super::MumbleClient;
    use mumble_protocol_2x::control::msgs;
    use std::collections::HashMap;

    #[test]
    fn resolves_sender_name_for_known_actor() {
        let mut user_names = HashMap::new();

        let mut user = msgs::UserState::new();
        user.set_session(12);
        user.set_name("Sam".to_string());
        MumbleClient::update_user_name_map(&user, &mut user_names);

        let mut text = msgs::TextMessage::new();
        text.set_actor(12);
        text.set_message("hello".to_string());

        let sender = MumbleClient::resolve_text_message_sender_name(&text, &user_names);
        assert_eq!(sender.as_deref(), Some("Sam"));
    }

    #[test]
    fn unknown_actor_has_no_sender_name() {
        let mut text = msgs::TextMessage::new();
        text.set_actor(99);
        text.set_message("hello".to_string());

        let sender = MumbleClient::resolve_text_message_sender_name(&text, &HashMap::new());
        assert!(sender.is_none());
    }

    #[test]
    fn user_remove_clears_sender_mapping() {
        let mut user_names = HashMap::new();
        user_names.insert(77, "Alex".to_string());

        MumbleClient::remove_user_from_map(77, &mut user_names);
        assert!(!user_names.contains_key(&77));
    }
}
