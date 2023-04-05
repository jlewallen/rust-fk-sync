use anyhow::Result;
use std::net::{Ipv4Addr, SocketAddrV4};
use std::sync::Arc;
use tokio::{
    net::UdpSocket,
    time::{sleep, Duration},
};
use tokio::{signal, task};

struct Discovery {}

impl Discovery {
    async fn run(&self) -> Result<()> {
        loop {
            sleep(Duration::from_secs(5)).await;
        }
    }
}

struct Server {}

impl Server {
    async fn run(&self) -> Result<()> {
        const IP_ALL: [u8; 4] = [0, 0, 0, 0];
        let addr = SocketAddrV4::new(IP_ALL.into(), 22144);
        let multi_addr = SocketAddrV4::new(Ipv4Addr::new(224, 1, 2, 3), 22143);

        println!("listening on {}", multi_addr);
        println!("server running on {}", addr);

        let socket = bind_multicast(&addr, &multi_addr)?;
        let socket = Arc::new(socket);
        let receiving = socket.clone();
        let sending = socket.clone();

        tokio::select! {
            res = task::spawn(async move { transmit(sending, "192.168.0.205:22144").await }) => {
                return res.map_err(|e| e.into()).and_then(|e| e)
            },
            res = task::spawn(async move { receive(receiving).await }) => {
                return res.map_err(|e| e.into()).and_then(|e| e)
            },
        }
    }
}

fn bind_multicast(addr: &SocketAddrV4, multi_addr: &SocketAddrV4) -> Result<UdpSocket> {
    use socket2::{Domain, Protocol, Socket, Type};

    assert!(multi_addr.ip().is_multicast(), "Must be multcast address");

    let socket = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;

    socket.set_reuse_address(true)?;
    socket.bind(&socket2::SockAddr::from(*addr))?;
    socket.set_multicast_loop_v4(true)?;
    socket.set_nonblocking(true)?;
    socket.join_multicast_v4(multi_addr.ip(), addr.ip())?;

    Ok(UdpSocket::from_std(socket.into())?)
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
            // You have to press Enter after pressing Ctrl+C for the program to terminate.
            // https://docs.rs/tokio/0.2.21/tokio/io/fn.stdin.html
            res = signal::ctrl_c() => {
                return res.map_err(|e| e.into())
            },
    })
}
