#[macro_use]
extern crate lazy_static;
extern crate hex;
extern crate socket2;

use socket2::{Domain, Protocol, Socket, Type};
use std::io;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;
use tokio::time::{sleep, Duration};
use tokio::{net::UdpSocket, sync::mpsc};

use anyhow::Result;

#[tokio::main]
async fn main() -> Result<()> {
    let sock = UdpSocket::bind("0.0.0.0:22144".parse::<SocketAddr>().unwrap()).await?;
    let r = Arc::new(sock);
    let s = r.clone();
    let (tx, mut rx) = mpsc::channel::<(Vec<u8>, SocketAddr)>(1_000);

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
    /*
    if true {
        let socket = UdpSocket::bind("192.168.0.100:22144")?;

        /*
        // Receives a single datagram message on the socket. If `buf` is too small to hold
        // the message, it will be cut off.
        let mut buf = [0; 10];
        let (amt, src) = socket.recv_from(&mut buf)?;

        // Redeclare `buf` as slice of the received data and send reverse data back to origin.
        let buf = &mut buf[..amt];
        buf.reverse();
        */

        for _ in 0..1000 {
            println!("send!");

            socket.send_to(&[0, 1, 2], "192.168.0.205:22134")?;

            let sleeping = time::Duration::from_secs(5);
            thread::sleep(sleeping);
        }
    } else {
        let addr = SocketAddr::new(*IPV4, PORT);
        let client_done = Arc::new(AtomicBool::new(false));

        multicast_listener("ipv4", client_done, addr);

        let sleeping = time::Duration::from_secs(60);
        thread::sleep(sleeping);
    }
    */

    Ok(())
}

pub const PORT: u16 = 22134;

lazy_static! {
    pub static ref IPV4: IpAddr = Ipv4Addr::new(224, 0, 0, 123).into();
}

fn new_socket(addr: &SocketAddr) -> io::Result<Socket> {
    let domain = if addr.is_ipv4() {
        Domain::IPV4
    } else {
        Domain::IPV6
    };

    let socket = Socket::new(domain, Type::DGRAM, Some(Protocol::UDP))?;

    socket.set_read_timeout(Some(Duration::from_millis(500)))?;

    Ok(socket)
}

/*
#[cfg(unix)]
fn bind_multicast(socket: &Socket, addr: &SocketAddr) -> io::Result<()> {
    socket.bind(&socket2::SockAddr::from(*addr))
}

fn join_multicast(addr: SocketAddr) -> io::Result<UdpSocket> {
    let ip_addr = addr.ip();
    let socket = new_socket(&addr)?;

    match ip_addr {
        IpAddr::V4(ref mdns_v4) => {
            socket.join_multicast_v4(mdns_v4, &Ipv4Addr::new(0, 0, 0, 0))?;
        }
        IpAddr::V6(ref mdns_v6) => {
            socket.join_multicast_v6(mdns_v6, 0)?;
            socket.set_only_v6(true)?;
        }
    };

    bind_multicast(&socket, &addr)?;

    Ok(socket.into())
}
*/

//        4b6af989-53334648-50202020-ff12410c

// 120a10-4b6af989-53334648-50202020-ff12410c

/*
fn multicast_listener(
    response: &'static str,
    client_done: Arc<AtomicBool>,
    addr: SocketAddr,
) -> JoinHandle<()> {
    let server_barrier = Arc::new(Barrier::new(2));
    let client_barrier = Arc::clone(&server_barrier);

    let join_handle = std::thread::Builder::new()
        .name(format!("{}:server", response))
        .spawn(move || {
            let listener = join_multicast(addr).expect("failed to create listener");

            server_barrier.wait();

            while !client_done.load(std::sync::atomic::Ordering::Relaxed) {
                let mut buf = [0u8; 64];

                match listener.recv_from(&mut buf) {
                    Ok((len, remote_addr)) => {
                        let data = &buf[..len];

                        let encoded = hex::encode(data);

                        println!(
                            "{}:server: got data: {} from: {}",
                            response, encoded, remote_addr
                        );
                    }
                    Err(err) => {
                        if err.kind() != std::io::ErrorKind::WouldBlock {
                            println!("{}:server: got an error: {} {}", response, err, err.kind());
                        }
                    }
                }
            }

            println!("{}:server: client is done", response);
        })
        .unwrap();

    client_barrier.wait();

    join_handle
}
*/
