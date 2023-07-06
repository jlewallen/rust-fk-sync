use anyhow::Result;
use std::{
    net::{Ipv4Addr, SocketAddr, SocketAddrV4},
    sync::Arc,
};
use tokio::{net::UdpSocket, sync::mpsc::Sender};
use tracing::*;

const MULTICAST_IP: [u8; 4] = [224, 1, 2, 3];
const MULTICAST_PORT: u16 = 22143;
const READ_BUFFER_SIZE: usize = 4096;
const DEFAULT_UDP_SERVER_PORT: u16 = 22144;

#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub struct DeviceId(pub String);

impl Into<String> for DeviceId {
    fn into(self) -> String {
        self.0
    }
}

#[derive(Clone, Debug)]
pub struct Discovered {
    pub device_id: DeviceId,
    pub http_addr: Option<SocketAddr>,
    pub udp_addr: Option<SocketAddr>,
}

impl Discovered {
    pub fn http_url(&self) -> Option<String> {
        self.http_addr.map(|addr| format!("http://{}/fk/v1", addr))
    }
}

#[derive(Default)]
pub struct Discovery {}

impl Discovery {
    pub async fn run(&self, publisher: Sender<Discovered>) -> Result<()> {
        let addr = SocketAddrV4::new(MULTICAST_IP.into(), MULTICAST_PORT);
        let receiving = Arc::new(self.bind(&addr)?);

        let mut buffer = vec![0u8; READ_BUFFER_SIZE];

        loop {
            let (len, addr) = receiving.recv_from(&mut buffer[..]).await?;
            trace!("{} bytes from {}", len, addr);

            let bytes = &buffer[0..len];
            let announced = Announce::parse(bytes)?;
            let discovered = Discovered {
                device_id: announced.device_id().clone(),
                http_addr: announced
                    .port()
                    .map(|port| SocketAddr::new(addr.ip(), port)),
                udp_addr: { Some(SocketAddr::new(addr.ip(), DEFAULT_UDP_SERVER_PORT)) },
            };

            trace!("discovered {:?}", discovered);

            publisher.send(discovered).await?;
        }
    }

    fn bind(&self, addr: &SocketAddrV4) -> Result<UdpSocket> {
        use socket2::{Domain, Protocol, Socket, Type};

        assert!(addr.ip().is_multicast(), "must be multcast address");

        let socket = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;

        info!("discovering on {}", addr);

        // This don't seem to be necessary when running a rust program from the
        // command line on Linux. If you omit them, then running a flutter
        // application that uses this library will fail to bind with an error
        // about the address being in use.
        socket.set_reuse_address(true)?;

        // Saving just in case this becomes a factor later, as the above did.
        // socket.set_multicast_loop_v4(true)?;

        // This is very important when using UdpSocket::from_std, otherwise
        // you'll see weird blocking behavior.
        socket.set_nonblocking(true)?;
        socket.bind(&socket2::SockAddr::from(*addr))?;
        socket.join_multicast_v4(addr.ip(), &Ipv4Addr::new(0, 0, 0, 0))?;

        Ok(UdpSocket::from_std(socket.into())?)
    }
}

pub enum Announce {
    Hello(DeviceId, u16),
    Bye(DeviceId),
}

impl Announce {
    fn parse(bytes: &[u8]) -> Result<Self> {
        const DEVICE_ID_TAG: u32 = 1;
        const PORT_TAG: u32 = 4;
        use quick_protobuf::BytesReader;

        let mut reader = BytesReader::from_bytes(bytes);
        let _size = reader.read_varint32(bytes)?;
        let tag = reader.next_tag(bytes)?;
        assert_eq!(tag >> 3, DEVICE_ID_TAG);
        let id_bytes = reader.read_bytes(bytes)?;
        let device_id = DeviceId(hex::encode(id_bytes));
        let port = if !reader.is_eof() {
            let tag = reader.next_tag(bytes)?;
            if tag >> 3 == PORT_TAG {
                reader.read_int32(bytes)?
            } else {
                80
            }
        } else {
            80
        };

        if reader.is_eof() {
            Ok(Announce::Hello(device_id, port as u16))
        } else {
            Ok(Announce::Bye(device_id))
        }
    }

    fn device_id(&self) -> &DeviceId {
        match self {
            Announce::Hello(id, _) => id,
            Announce::Bye(id) => id,
        }
    }

    fn port(&self) -> Option<u16> {
        match self {
            Announce::Hello(_, port) => Some(*port),
            Announce::Bye(_) => None,
        }
    }
}
