// extern crate hex;
// extern crate lazy_static;
// extern crate socket2;

use anyhow::Result;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::{
    net::UdpSocket,
    sync::mpsc,
    time::{sleep, Duration},
};

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

    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    simple_round_trip().await?;

    Ok(())
}
