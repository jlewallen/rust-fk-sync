use anyhow::Result;
use discovery::{Discovered, Discovery, Server};
use std::sync::Arc;
use tokio::{signal, sync::mpsc};
use tracing::*;
use tracing_subscriber::prelude::*;

#[tokio::main]
async fn main() -> Result<()> {
    fn get_rust_log() -> String {
        std::env::var("RUST_LOG").unwrap_or_else(|_| "info".into())
    }

    tracing_subscriber::registry()
        .with(tracing_subscriber::EnvFilter::new(get_rust_log()))
        .with(tracing_subscriber::fmt::layer())
        .init();

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

    Ok(tokio::select! {
        _ = discovery.run(tx) => {},
        _ = server.run() => {},
        _ = pump => {},
        res = signal::ctrl_c() => {
            return res.map_err(|e| e.into())
        },
    })
}
