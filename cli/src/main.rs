use anyhow::{Context, Result};
use clap::{Args, Parser, Subcommand};
use query::portal::{LoginPayload, PortalError, Tokens};
use std::sync::Arc;
use tokio::{signal, sync::mpsc};
use tracing::*;
use tracing_subscriber::prelude::*;

use discovery::{DeviceId, Discovered, Discovery};
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
    Sync(SyncCommand),
}

#[derive(Args)]
pub struct SyncCommand {
    #[arg(long, default_value = None)]
    discover_device_id: Option<String>,
    #[arg(long, default_value = None)]
    discover_ip: Option<String>,
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
                    email: std::env::var("FK_EMAIL").context("FK_EMAIL is required.")?,
                    password: std::env::var("FK_PASSWORD").context("FK_PASSWORD is required.")?,
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

                let ourselves = client.query_ourselves().await.context("GET /user")?;

                println!("{:?}", ourselves);

                let transmission_token = client
                    .issue_transmission_token()
                    .await
                    .context("GET /transmission-token")?;

                println!("{:?}", transmission_token);
            }

            Ok(())
        }
        Some(Commands::QueryDevice) => {
            let client = query::device::Client::new()?;
            let status = client
                .query_status("192.168.0.205")
                .await
                .context("Querying 192.168.0.205")?;

            info!("{:?}", status);

            Ok(())
        }
        Some(Commands::Sync(command)) => {
            let (transfer_publish, mut transfer_events) = mpsc::channel::<ServerEvent>(32);
            let server = Arc::new(Server::new(transfer_publish));
            let discovery = Discovery::default();
            let (tx, mut rx) = mpsc::channel::<Discovered>(32);

            let ignore = tokio::spawn({
                let server = server.clone();
                async move {
                    while let Some(d) = transfer_events.recv().await {
                        match d {
                            ServerEvent::Completed(device_id) => {
                                info!("{:?} pausing", device_id);
                                tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                                info!("{:?} removing sync", device_id);
                                server.cancel(device_id).await.expect("Error cancelling")
                            }
                            _ => trace!("{:?}", d),
                        }
                    }
                }
            });

            let pump = tokio::spawn({
                let server = server.clone();
                async move {
                    while let Some(d) = rx.recv().await {
                        if let Some(http_addr) = d.http_addr {
                            if http_addr.port() == 80 || http_addr.port() == 0 {
                                info!("{:?}", d);
                                server.sync(d).await.expect("Error initiating sync");
                            } else {
                                debug!("{:?} (ignored)", d);
                            }
                        }
                    }
                }
            });

            match (command.discover_device_id, command.discover_ip) {
                (Some(device_id), Some(ip)) => {
                    let _begin = tokio::spawn({
                        let server = server.clone();
                        async move {
                            tokio::time::sleep(std::time::Duration::from_secs(5)).await;

                            server
                                .sync(Discovered {
                                    device_id: DeviceId(device_id),
                                    http_addr: Some(
                                        format!("{}:80", ip)
                                            .parse()
                                            .expect("Parsing http_addr failed"),
                                    ),
                                    udp_addr: Some(
                                        format!("{}:22144", ip)
                                            .parse()
                                            .expect("Parsing udp_addr failed"),
                                    ),
                                })
                                .await
                                .expect("error initiating sync");
                        }
                    });
                }
                _ => {}
            }

            #[allow(clippy::unit_arg)]
            Ok(tokio::select! {
                _ = discovery.run(tx) => {},
                _ = server.run() => {},
                _ = pump => {},
                _ = ignore=> {},
                res = signal::ctrl_c() => {
                    return res.map_err(|e| e.into())
                },
            })
        }
        _ => Ok(()),
    }
}
