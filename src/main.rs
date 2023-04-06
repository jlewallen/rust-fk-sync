use anyhow::Result;
use std::{
    collections::HashMap,
    net::{Ipv4Addr, SocketAddr, SocketAddrV4},
    sync::Arc,
};
use tokio::{
    net::{ToSocketAddrs, UdpSocket},
    signal,
    sync::mpsc,
    sync::mpsc::Sender,
    sync::Mutex,
    time::Instant,
    // task,
    // time::{sleep, Duration},
};
use tracing::*;
use tracing_subscriber::prelude::*;

#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub struct DeviceId(String);

#[derive(Clone, Debug)]
pub struct Discovered {
    device_id: DeviceId,
    addr: SocketAddr,
}

fn get_device_id(bytes: &[u8]) -> Result<DeviceId> {
    use quick_protobuf::BytesReader;

    let mut reader = BytesReader::from_bytes(bytes);
    let size = reader.read_varint32(bytes)?;
    match size {
        18 => {
            let tag = reader.next_tag(bytes)?;
            assert_eq!(tag >> 3, 1);

            let id_bytes = reader.read_bytes(bytes)?;
            assert_eq!(id_bytes.len(), 16);

            Ok(DeviceId(hex::encode(id_bytes)))
        }
        _ => todo!(),
    }
}

enum DeviceState {
    JustDiscovered,
    Querying,
}

#[allow(dead_code)]
struct ConnectedDevice {
    discovered: Discovered,
    state: DeviceState,
    activity: Instant,
}

impl ConnectedDevice {
    fn should_query(&mut self) -> bool {
        match self.state {
            DeviceState::JustDiscovered => {
                self.state = DeviceState::Querying;
                self.activity = Instant::now();
                true
            }
            DeviceState::Querying => {
                if Instant::now() - self.activity > std::time::Duration::from_secs(30) {
                    self.activity = Instant::now();
                    true
                } else {
                    false
                }
            }
        }
    }
}

#[derive(Debug)]
enum ServerCommand {
    StartSyncing(Discovered),
    Reply(SocketAddr, Message),
}

struct Server {
    port: u16,
    sender: Arc<Mutex<Option<Sender<ServerCommand>>>>,
}

impl Default for Server {
    fn default() -> Self {
        Self {
            port: 22144,
            sender: Arc::new(Mutex::new(None)),
        }
    }
}

impl Server {
    pub async fn run(&self) -> Result<()> {
        const IP_ALL: [u8; 4] = [0, 0, 0, 0];
        let addr = SocketAddrV4::new(IP_ALL.into(), self.port);
        let receiving = Arc::new(self.bind(&addr)?);
        let sending = receiving.clone();

        info!("listening on {}", addr);

        let mut devices = HashMap::<DeviceId, ConnectedDevice>::default();
        let (tx, mut rx) = mpsc::channel::<ServerCommand>(32);

        let pump = tokio::spawn({
            let mut locked = self.sender.lock().await;
            *locked = Some(tx.clone());
            async move {
                while let Some(cmd) = rx.recv().await {
                    match &cmd {
                        ServerCommand::StartSyncing(discovered) => {
                            info!("{:?}", cmd);

                            let entry = devices.entry(discovered.device_id.clone());
                            let entry = entry.or_insert_with(|| ConnectedDevice {
                                discovered: discovered.clone(),
                                state: DeviceState::JustDiscovered,
                                activity: Instant::now(),
                            });

                            if entry.should_query() {
                                transmit(&sending, &discovered.addr, &Message::Query)
                                    .await
                                    .expect("send failed");
                            }
                        }
                        ServerCommand::Reply(addr, reply) => {
                            info!("{:?}", reply);

                            match reply {
                                Message::Query => todo!(),
                                Message::Statistics { tail } => {
                                    info!("requiring {}", tail);

                                    let reply = Message::Require {
                                        first: 0,
                                        // tail: *tail,
                                        tail: 100,
                                    };

                                    transmit(&sending, &addr, &reply)
                                        .await
                                        .expect("send failed");
                                }
                                #[allow(unused_variables)]
                                Message::Require { first, tail } => todo!(),
                                #[allow(unused_variables)]
                                Message::Records { first, records } => {}
                            }
                        }
                    }
                }
            }
        });

        let receive = tokio::spawn({
            async move {
                let mut buffer = vec![0u8; 4096];

                loop {
                    let (len, addr) = receiving
                        .recv_from(&mut buffer[..])
                        .await
                        .expect("recv failed");

                    debug!("{} bytes from {}", len, addr);

                    let message = Message::read(&buffer[0..len]).expect("parse failed");

                    tx.send(ServerCommand::Reply(addr, message))
                        .await
                        .expect("send self failed");
                }
            }
        });

        tokio::select! {
            _ = pump => {
                println!("server pump done");
                Ok(())
            },
            _ = receive => {
                println!("receive pump done");
                Ok(())
            },
        }
    }

    pub async fn sync(&self, discovered: Discovered) -> Result<()> {
        self.send(ServerCommand::StartSyncing(discovered)).await
    }

    async fn send(&self, cmd: ServerCommand) -> Result<()> {
        let locked = self.sender.lock().await;
        Ok(locked.as_ref().expect("sender required").send(cmd).await?)
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

async fn transmit<A>(tx: &Arc<UdpSocket>, addr: &A, m: &Message) -> Result<()>
where
    A: ToSocketAddrs + std::fmt::Debug,
{
    info!("{:?} to {:?}", m, addr);

    let mut buffer = Vec::new();
    m.write(&mut buffer)?;

    let len = tx.send_to(&buffer, addr).await?;
    debug!("{:?} bytes to {:?}", len, addr);

    Ok(())
}

#[derive(Default)]
struct Discovery {}

impl Discovery {
    pub async fn run(&self, publisher: Sender<Discovered>) -> Result<()> {
        let addr = SocketAddrV4::new(Ipv4Addr::new(224, 1, 2, 3), 22143);
        let receiving = Arc::new(self.bind(&addr)?);

        info!("discovering on {}", addr);

        let mut buffer = vec![0u8; 4096];

        loop {
            let (len, addr) = receiving.recv_from(&mut buffer[..]).await?;
            debug!("{} bytes from {}", len, addr);

            let bytes = &buffer[0..len];
            let device_id = get_device_id(bytes)?;
            let discovered = Discovered {
                device_id,
                addr: {
                    let mut addr = addr;
                    addr.set_port(22144);
                    addr
                },
            };

            debug!("discovered {:?}", discovered);

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

fn get_rust_log() -> String {
    std::env::var("RUST_LOG").unwrap_or_else(|_| "info".into())
}

#[tokio::main]
async fn main() -> Result<()> {
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
        _ = discovery.run(tx) => {
            println!("discovery done")
        },
        _ = server.run() => {
            println!("server done")
        },
        _ = pump => {
            println!("pump done")
        },
        res = signal::ctrl_c() => {
            return res.map_err(|e| e.into())
        },
    })
}

struct RawRecord(Vec<u8>);

impl std::fmt::Debug for RawRecord {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("RawRecord").field(&self.0.len()).finish()
    }
}

#[derive(Debug)]
enum Message {
    Query,
    Statistics { tail: u64 },
    Require { first: u64, tail: u64 },
    Records { first: u64, records: Vec<RawRecord> },
}

impl Message {
    fn read(bytes: &[u8]) -> Result<Self> {
        use quick_protobuf::reader::BytesReader;

        let mut reader = BytesReader::from_bytes(bytes);
        let kind = reader.read_fixed32(bytes)?;

        match kind {
            0 => Ok(Self::Query {}),
            1 => {
                let tail = reader.read_fixed32(bytes)? as u64;
                Ok(Self::Statistics { tail })
            }
            2 => {
                let first = reader.read_fixed32(bytes)? as u64;
                let tail = reader.read_fixed32(bytes)? as u64;
                Ok(Self::Require { first, tail })
            }
            3 => {
                let first = reader.read_fixed32(bytes)? as u64;

                let mut records: Vec<RawRecord> = Vec::new();
                while !reader.is_eof() {
                    let record = reader.read_bytes(bytes)?;
                    records.push(RawRecord(record.into()));
                }

                Ok(Self::Records { first, records })
            }
            _ => todo!(),
        }
    }

    fn write(&self, bytes: &mut Vec<u8>) -> Result<()> {
        use quick_protobuf::writer::Writer;

        let mut writer = Writer::new(bytes);

        match self {
            Message::Query => {
                writer.write_fixed32(0)?;
                Ok(())
            }
            Message::Statistics { tail } => {
                writer.write_fixed32(1)?;
                writer.write_fixed32(*tail as u32)?;
                Ok(())
            }
            Message::Require { first, tail } => {
                writer.write_fixed32(2)?;
                writer.write_fixed32(*first as u32)?;
                writer.write_fixed32(*tail as u32)?;
                Ok(())
            }
            Message::Records { first, records } => {
                writer.write_fixed32(3)?;
                writer.write_fixed32(*first as u32)?;

                assert!(records.len() == 0); // Laziness

                Ok(())
            }
        }
    }
}
