use anyhow::Result;
use async_trait::async_trait;
use std::{
    net::{SocketAddr, SocketAddrV4},
    sync::Arc,
};
use tokio::net::UdpSocket;
use tracing::*;

use crate::proto::{Message, MessageCodec};

const DEFAULT_PORT: u16 = 22144;
const IP_ALL: [u8; 4] = [0, 0, 0, 0];

#[derive(Debug)]
pub struct TransportMessage(pub (SocketAddr, Message));

#[async_trait]
pub trait SendTransport: Send + Sync + 'static {
    async fn send(&self, message: TransportMessage) -> Result<()>;
}

#[async_trait]
pub trait ReceiveTransport: Send + Sync + 'static {
    async fn recv(&self) -> Result<Option<Vec<TransportMessage>>>;
}

#[async_trait]
pub trait Transport {
    type Send: SendTransport;
    type Receive: ReceiveTransport;

    async fn open(&self) -> Result<(Self::Send, Self::Receive)>;
}

#[derive(Clone)]
pub struct OpenUdp {
    socket: Arc<UdpSocket>,
}

#[async_trait]
impl SendTransport for OpenUdp {
    async fn send(&self, message: TransportMessage) -> Result<()> {
        let TransportMessage((addr, message)) = message;
        let mut buffer = Vec::new();
        message.write(&mut buffer)?;

        debug!("{:?} Sending {:?}", addr, buffer.len());
        self.socket.send_to(&buffer, addr).await?;

        Ok(())
    }
}

#[async_trait]
impl ReceiveTransport for OpenUdp {
    async fn recv(&self) -> Result<Option<Vec<TransportMessage>>> {
        let mut codec = MessageCodec::default();
        let mut batch: Vec<TransportMessage> = Vec::new();

        self.socket.readable().await?;

        loop {
            let mut buffer = vec![0u8; 4096];

            match self.socket.try_recv_from(&mut buffer[..]) {
                Ok((len, addr)) => {
                    trace!("{:?} Received {:?}", addr, len);

                    match codec.try_read(&buffer[..len])? {
                        Some(message) => {
                            if let Message::Batch {
                                flags: _flags,
                                errors,
                            } = message
                            {
                                if errors > 0 {
                                    warn!("{:?} Batch ({:?} Errors)", addr, errors)
                                } else {
                                    info!("{:?} Batch", addr)
                                }
                            }

                            batch.push(TransportMessage((addr, message)));
                        }
                        None => {}
                    }
                }
                Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    return Ok(Some(batch));
                }
                Err(e) => return Err(e.into()),
            }
        }
    }
}

pub struct UdpTransport {
    port: u16,
}

impl UdpTransport {
    pub fn new() -> Self {
        Self { port: DEFAULT_PORT }
    }
}

#[async_trait]
impl Transport for UdpTransport {
    type Send = OpenUdp;
    type Receive = OpenUdp;

    async fn open(&self) -> Result<(OpenUdp, OpenUdp)> {
        let listening_addr = SocketAddrV4::new(IP_ALL.into(), self.port);
        let socket = Arc::new(bind(&listening_addr)?);

        info!("listening on {}", listening_addr);

        let sender = OpenUdp { socket };

        let receiver = sender.clone();

        Ok((sender, receiver))
    }
}

fn bind(addr: &SocketAddrV4) -> Result<UdpSocket> {
    use socket2::{Domain, Protocol, Socket, Type};

    let socket = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;

    socket.set_reuse_address(true)?;
    socket.set_nonblocking(true)?;
    socket.bind(&socket2::SockAddr::from(*addr))?;

    Ok(UdpSocket::from_std(socket.into())?)
}
