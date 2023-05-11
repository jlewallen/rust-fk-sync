use anyhow::Result;
use std::{
    net::{Ipv4Addr, SocketAddr, SocketAddrV4},
    sync::Arc,
};
use tokio::{net::UdpSocket, sync::mpsc::Sender};
use tracing::*;

#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub struct DeviceId(pub String);

#[derive(Clone, Debug)]
pub struct Discovered {
    pub device_id: DeviceId,
    pub addr: SocketAddr,
}

#[derive(Default)]
pub struct Discovery {}

impl Discovery {
    pub async fn run(&self, publisher: Sender<Discovered>) -> Result<()> {
        let addr = SocketAddrV4::new(Ipv4Addr::new(224, 1, 2, 3), 22143);
        let receiving = Arc::new(self.bind(&addr)?);

        info!("discovering on {}", addr);

        let mut buffer = vec![0u8; 4096];

        loop {
            let (len, addr) = receiving.recv_from(&mut buffer[..]).await?;
            trace!("{} bytes from {}", len, addr);

            let bytes = &buffer[0..len];
            let announced = Announce::parse(bytes)?;
            let discovered = Discovered {
                device_id: announced.device_id().clone(),
                addr: {
                    let mut addr = addr;
                    addr.set_port(22144);
                    addr
                },
            };

            trace!("discovered {:?}", discovered);

            publisher.send(discovered).await?;
        }
    }

    fn bind(&self, addr: &SocketAddrV4) -> Result<UdpSocket> {
        use socket2::{Domain, Protocol, Socket, Type};

        assert!(addr.ip().is_multicast(), "must be multcast address");

        let socket = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;

        // Unnecessary for our use case, though good to know it's around.
        // socket.set_reuse_address(true)?;
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
    Hello(DeviceId),
    Bye(DeviceId),
}

impl Announce {
    fn parse(bytes: &[u8]) -> Result<Self> {
        use quick_protobuf::BytesReader;

        let mut reader = BytesReader::from_bytes(bytes);
        let size = reader.read_varint32(bytes)?;
        let tag = reader.next_tag(bytes)?;
        assert_eq!(tag >> 3, 1);
        let id_bytes = reader.read_bytes(bytes)?;
        assert_eq!(id_bytes.len(), 16);
        let device_id = DeviceId(hex::encode(id_bytes));

        if size == 18 {
            Ok(Announce::Hello(device_id))
        } else {
            Ok(Announce::Bye(device_id))
        }
    }

    fn device_id(&self) -> &DeviceId {
        match self {
            Announce::Hello(id) => id,
            Announce::Bye(id) => id,
        }
    }
}
