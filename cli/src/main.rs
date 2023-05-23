use anyhow::Result;
use clap::{Parser, Subcommand};
use query::portal::{LoginPayload, PortalError, Tokens};
use std::sync::Arc;
use tokio::{signal, sync::mpsc};
use tracing::*;
use tracing_subscriber::prelude::*;

use discovery::{Discovered, Discovery};
use sync::{Server, ServerEvent};

#[derive(Parser)]
#[command(author, version, about, long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand)]
pub enum Commands {
    QueryDevice,
    QueryPortal,
    Sync,
}

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

    let cli = Cli::parse();

    match cli.command {
        Some(Commands::QueryPortal) => {
            let client = query::portal::Client::new()?;
            let tokens = client
                .login(LoginPayload {
                    email: std::env::var("FK_EMAIL")?,
                    password: std::env::var("FK_PASSWORD")?,
                })
                .await?;

            if let Some(tokens) = tokens {
                let broken_client = client.to_authenticated(Tokens {
                    token: "INVALID".to_string(),
                })?;
                match broken_client.query_ourselves().await {
                    Ok(_) => panic!("Whoa, how'd that happen?"),
                    Err(PortalError::HttpStatus(status)) => info!("http status: {:?}", status),
                    Err(e) => info!("{:?}", e),
                };

                let client = client.to_authenticated(tokens)?;

                let ourselves = client.query_ourselves().await?;

                println!("{:?}", ourselves);

                let transmission_token = client.issue_transmission_token().await?;

                println!("{:?}", transmission_token);
            }

            Ok(())
        }
        Some(Commands::QueryDevice) => {
            let client = query::device::Client::new()?;
            let status = client.query_status("192.168.0.205").await?;

            info!("{:?}", status);

            Ok(())
        }
        Some(Commands::Sync) => {
            let (transfer_publish, mut _transfer_events) = mpsc::channel::<ServerEvent>(32);
            let server = Arc::new(Server::new(transfer_publish));
            let discovery = Discovery::default();
            let (tx, mut rx) = mpsc::channel::<Discovered>(0);

            let pump = tokio::spawn({
                let server = server.clone();
                async move {
                    while let Some(d) = rx.recv().await {
                        if d.http_addr.port() == 80 || d.http_addr.port() == 0 {
                            info!("{:?}", d);
                            server.sync(d).await.expect("error initiating sync");
                        } else {
                            debug!("{:?} (ignored)", d);
                        }
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
        _ => Ok(()),
    }
}
