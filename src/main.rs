use anyhow::Result;
use std::net::{Ipv4Addr, SocketAddrV4};
use std::sync::Arc;
use tokio::{
    net::UdpSocket,
    time::{sleep, Duration},
};
use tokio::{signal, task};

struct Server {}

impl Server {
    async fn run(&self) -> Result<()> {
        const IP_ALL: [u8; 4] = [0, 0, 0, 0];
        let addr = SocketAddrV4::new(IP_ALL.into(), 22144);
        let receiving = Arc::new(self.bind(&addr)?);
        let sending = receiving.clone();

        tokio::select! {
            res = task::spawn(async move { transmit(sending, "192.168.0.205:22144").await }) => {
                return res.map_err(|e| e.into()).and_then(|e| e)
            },
            res = task::spawn(async move { receive(receiving).await }) => {
                return res.map_err(|e| e.into()).and_then(|e| e)
            },
        }
    }

    fn bind(&self, addr: &SocketAddrV4) -> Result<UdpSocket> {
        use socket2::{Domain, Protocol, Socket, Type};

        let socket = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;

        socket.set_reuse_address(true)?;
        socket.set_nonblocking(true)?;
        socket.bind(&socket2::SockAddr::from(*addr))?;

        Ok(UdpSocket::from_std(socket.into())?)
    }
}

async fn receive(rx: Arc<UdpSocket>) -> Result<()> {
    let mut buffer = vec![0u8; 4096];

    loop {
        let (len, addr) = rx.recv_from(&mut buffer[..]).await?;
        println!("{} received bytes from {}", len, addr);
    }
}

async fn transmit(tx: Arc<UdpSocket>, addr: &str) -> Result<()> {
    loop {
        let len = tx.send_to(&[0, 1, 2], addr).await?;
        println!("{:?} bytes sent", len);

        sleep(Duration::from_secs(5)).await;
    }
}

struct Discovery {}

impl Discovery {
    async fn run(&self) -> Result<()> {
        let addr = SocketAddrV4::new(Ipv4Addr::new(224, 1, 2, 3), 22143);

        println!("discovering on {}", addr);

        let receiving = Arc::new(self.bind(&addr)?);

        tokio::select! {
            res = task::spawn(async move { receive_discoveries(receiving).await }) => {
                return res.map_err(|e| e.into()).and_then(|e| e)
            },
        }
    }

    fn bind(&self, addr: &SocketAddrV4) -> Result<UdpSocket> {
        use socket2::{Domain, Protocol, Socket, Type};

        assert!(addr.ip().is_multicast(), "Must be multcast address");

        let socket = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;

        socket.set_reuse_address(true)?;
        socket.set_multicast_loop_v4(true)?;
        socket.set_nonblocking(true)?;

        socket.bind(&socket2::SockAddr::from(*addr))?;
        socket.join_multicast_v4(addr.ip(), &Ipv4Addr::new(0, 0, 0, 0))?;

        Ok(UdpSocket::from_std(socket.into())?)
    }
}

async fn receive_discoveries(rx: Arc<UdpSocket>) -> Result<()> {
    let mut buffer = vec![0u8; 4096];

    loop {
        let (len, addr) = rx.recv_from(&mut buffer[..]).await?;
        println!("{} received bytes from {}", len, addr);
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let server = Server {};
    let discovery = Discovery {};

    Ok(tokio::select! {
        _ = discovery.run() => {
            println!("discovery done")
        },
        _ = server.run() => {
            println!("server done")
        },
        res = signal::ctrl_c() => {
            return res.map_err(|e| e.into())
        },
    })
}
