use crate::{
    broker::BrokerMessage,
    types::{ConnectAckPacket, Packet, ProtocolError, SubscribeAckPacket},
    MqttCodec,
};
use futures::{
    stream::{SplitSink, SplitStream},
    FutureExt, SinkExt, StreamExt,
};
use std::time::Duration;
use tokio::{
    io::{AsyncRead, AsyncWrite},
    sync::mpsc::{self, Receiver, Sender},
    time,
};
use tokio_util::codec::Framed;

pub struct UnconnectedClient<T> {
    framed_stream: Framed<T, MqttCodec>,
    broker_tx: Sender<BrokerMessage>,
}

impl<T: AsyncRead + AsyncWrite + Unpin> UnconnectedClient<T> {
    pub fn new(framed_stream: Framed<T, MqttCodec>, broker_tx: Sender<BrokerMessage>) -> Self {
        Self { framed_stream, broker_tx }
    }

    pub async fn handshake(mut self) -> Result<Client<T>, ProtocolError> {
        let first_packet = time::timeout(Duration::from_secs(2), self.framed_stream.next())
            .await
            .map_err(|_| ProtocolError::ConnectTimedOut)?;

        println!("got a packet: {:?}", first_packet);

        match first_packet {
            Some(Ok(Packet::Connect(connect_packet))) => {
                let (sender, receiver) = mpsc::channel(5);

                // TODO - Use a UUID or some other random unique ID
                let client_id = if connect_packet.client_id.is_empty() {
                    "EMPTY_CLIENT_ID".to_string()
                } else {
                    connect_packet.client_id
                };

                let self_tx = sender.clone();

                self.broker_tx
                    .send(BrokerMessage::NewClient(client_id.clone(), sender))
                    .await
                    .expect("Couldn't send NewClient message to broker");

                Ok(Client::new(client_id, self.framed_stream, self.broker_tx, receiver, self_tx))
            },
            Some(Ok(_)) => Err(ProtocolError::FirstPacketNotConnect),
            Some(Err(e)) => Err(ProtocolError::MalformedPacket(e)),
            None => {
                // TODO(bschwind) - Technically end of stream?
                Err(ProtocolError::FirstPacketNotConnect)
            },
        }
    }
}

#[derive(Debug)]
pub enum ClientMessage {
    ConnectAck(ConnectAckPacket),
    SubscribeAck(SubscribeAckPacket),
    Disconnect,
}

pub struct Client<T: AsyncRead + AsyncWrite + Unpin> {
    id: String,
    framed_stream: Framed<T, MqttCodec>,
    broker_tx: Sender<BrokerMessage>,
    broker_rx: Receiver<ClientMessage>,
    self_tx: Sender<ClientMessage>,
}

impl<T: AsyncRead + AsyncWrite + Unpin> Client<T> {
    pub fn new(
        id: String,
        framed_stream: Framed<T, MqttCodec>,
        broker_tx: Sender<BrokerMessage>,
        broker_rx: Receiver<ClientMessage>,
        self_tx: Sender<ClientMessage>,
    ) -> Self {
        Self { id, framed_stream, broker_tx, broker_rx, self_tx }
    }

    async fn handle_socket_reads(
        mut stream: SplitStream<Framed<T, MqttCodec>>,
        client_id: String,
        mut broker_tx: Sender<BrokerMessage>,
        _self_tx: Sender<ClientMessage>,
    ) {
        while let Some(frame) = stream.next().await {
            match frame {
                Ok(frame) => {
                    println!("Got a frame: {:#?}", frame);

                    match frame {
                        Packet::Subscribe(packet) => {
                            broker_tx
                                .send(BrokerMessage::Subscribe(client_id.clone(), packet))
                                .await
                                .expect("Couldn't send Subscribe message to broker");
                        },
                        _ => {},
                    }
                },
                Err(err) => {
                    println!("Error while reading frame: {:?}", err);
                    break;
                },
            }
        }
    }

    async fn handle_socket_writes(
        mut sink: SplitSink<Framed<T, MqttCodec>, Packet>,
        mut broker_rx: Receiver<ClientMessage>,
    ) {
        while let Some(frame) = broker_rx.recv().await {
            match frame {
                ClientMessage::ConnectAck(packet) => {
                    sink.send(Packet::ConnectAck(packet))
                        .await
                        .expect("Couldn't forward packet to framed socket");
                },
                ClientMessage::SubscribeAck(packet) => {
                    sink.send(Packet::SubscribeAck(packet))
                        .await
                        .expect("Couldn't forward packet to framed socket");
                },
                ClientMessage::Disconnect => println!("broker told the client to disconnect"),
            }
        }
    }

    pub async fn run(self) {
        let (sink, stream) = self.framed_stream.split();

        let task_rx =
            Self::handle_socket_reads(stream, self.id, self.broker_tx, self.self_tx).fuse();
        let task_tx = Self::handle_socket_writes(sink, self.broker_rx).fuse();

        futures::pin_mut!(task_rx, task_tx);

        futures::select! {
            _ = task_rx => println!("rx"),
            _ = task_tx => println!("tx"),
            complete => println!("complete"),
        }
    }
}