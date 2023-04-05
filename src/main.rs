// extern crate hex;
// extern crate lazy_static;
// extern crate socket2;

use anyhow::Result;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use std::sync::Arc;
use tokio::{
    net::UdpSocket,
    sync::mpsc,
    time::{sleep, Duration},
};
use tokio::{signal, task};

async fn simple_round_trip() -> Result<()> {
    let sock = UdpSocket::bind("0.0.0.0:22144".parse::<SocketAddr>().unwrap()).await?;
    let r = Arc::new(sock);
    let s = r.clone();
    let (tx, /* mut */ _rx) = mpsc::channel::<(Vec<u8>, SocketAddr)>(1_000);

    tokio::spawn(async move {
        loop {
            let len = s
                .send_to(&[0, 1, 2], "192.168.0.205:22134")
                .await
                .expect("send_to failed");

            println!("{:?} bytes sent", len);

            sleep(Duration::from_secs(5)).await;
        }
        /*
        while let Some((bytes, addr)) = rx.recv().await {
            let len = s.send_to(&bytes, &addr).await.unwrap();
            println!("{:?} bytes sent", len);
        }
        */
    });

    let mut buf = [0; 128];
    loop {
        let (len, addr) = r.recv_from(&mut buf).await?;
        println!("{:?} bytes received from {:?}", len, addr);
        tx.send((buf[..len].to_vec(), addr)).await.unwrap();
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

        sleep(Duration::from_secs(1)).await;
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
    if false {
        simple_round_trip().await?;
    }

    const IP_ALL: [u8; 4] = [0, 0, 0, 0];
    let addr = SocketAddrV4::new(IP_ALL.into(), 22144);
    let multi_addr = SocketAddrV4::new(Ipv4Addr::new(224, 1, 2, 3), 22144);
    let socket = bind_multicast(&addr, &multi_addr)?;
    let socket = Arc::new(socket);
    let receiving = socket.clone();
    let sending = socket.clone();

    tokio::select! {
        res = task::spawn(async move { transmit(sending, "192.168.0.205:22134").await }) => {
            return res.map_err(|e| e.into()).and_then(|e| e)
        },
        res = task::spawn(async move { receive(receiving).await }) => {
            return res.map_err(|e| e.into()).and_then(|e| e)
        },
        // You have to press Enter after pressing Ctrl+C for the program to terminate.
        // https://docs.rs/tokio/0.2.21/tokio/io/fn.stdin.html
        res = signal::ctrl_c() => {
            return res.map_err(|e| e.into())
        }
    };
}
