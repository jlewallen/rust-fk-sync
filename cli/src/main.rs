use anyhow::Result;
use query::portal::LoginPayload;
use std::sync::Arc;
use tokio::{signal, sync::mpsc};
use tracing::*;
use tracing_subscriber::prelude::*;

use discovery::{Discovered, Discovery};
use sync::Server;

#[tokio::main]
async fn main() -> Result<()> {
    fn get_rust_log() -> String {
        let mut original = std::env::var("RUST_LOG").unwrap_or_else(|_| "info".into());

        if !original.contains("hyper=") {
            original.push_str(",hyper=info");
        }

        if !original.contains("reqwest=") {
            original.push_str(",reqwest=info");
        }

        original
    }

    tracing_subscriber::registry()
        .with(tracing_subscriber::EnvFilter::new(get_rust_log()))
        .with(tracing_subscriber::fmt::layer())
        .init();

    if false {
        /*
        const TEST_IP: [u8; 4] = [192, 168, 0, 205];
        let client = query::device::Client::new()?;
        let _status = client.query_status(TEST_IP.into()).await?;
        */

        let client = query::portal::Client::new()?;
        let tokens = client
            .login(LoginPayload {
                email: std::env::var("FK_EMAIL")?,
                password: std::env::var("FK_PASSWORD")?,
            })
            .await?;

        if let Some(tokens) = tokens {
            let client = client.to_authenticated(tokens)?;

            let ourselves = client.query_ourselves().await?;

            println!("{:?}", ourselves);

            let transmission_token = client.issue_transmission_token().await?;

            println!("{:?}", transmission_token);
        }

        panic!();
    }

    let server = Arc::new(Server::default());
    let discovery = Discovery::default();
    let (tx, mut rx) = mpsc::channel::<Discovered>(32);

    let pump = tokio::spawn({
        let server = server.clone();
        async move {
            while let Some(d) = rx.recv().await {
                info!("{:?}", d);
                server.sync(d).await.expect("error initiating sync");
            }
        }
    });

    #[allow(clippy::unit_arg)]
    Ok(tokio::select! {
        _ = discovery.run(tx) => {},
        _ = server.run() => {},
        _ = pump => {},
        res = signal::ctrl_c() => {
            return res.map_err(|e| e.into())
        },
    })
}
