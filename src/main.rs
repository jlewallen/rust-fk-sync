use anyhow::Result;
use range_set_blaze::prelude::*;
use std::{
    collections::HashMap,
    net::{Ipv4Addr, SocketAddr, SocketAddrV4},
    sync::Arc,
    time::Duration,
};
use tokio::{
    net::{ToSocketAddrs, UdpSocket},
    signal,
    sync::mpsc,
    sync::mpsc::Sender,
    sync::Mutex,
    time::{self, Instant},
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
        20 => {
            warn!("{}", hex::encode(bytes));

            todo!()
        }
        _ => unimplemented!("{} bytes announce", size),
    }
}

#[derive(Debug, Clone)]
struct RecordRange((u64, u64));

impl RecordRange {
    fn new(h: u64, t: u64) -> Self {
        Self((h, t))
    }

    /*
    fn all() -> Self {
        Self((0, u32::MAX as u64))
    }
    */

    fn head(&self) -> u64 {
        self.0 .0
    }

    fn tail(&self) -> u64 {
        self.0 .1
    }

    fn into_set(&self) -> RangeSetBlaze<u64> {
        RangeSetBlaze::from_iter([self.head()..=self.tail() - 1])
    }
}

#[derive(Debug)]
enum DeviceState {
    Discovered,
    Learning,
    Receiving(RecordRange),
    Synced,
}

impl DeviceState {}

#[derive(Debug)]
#[allow(dead_code)]
struct ConnectedDevice {
    batch_size: u64,
    discovered: Discovered,
    state: DeviceState,
    activity: Instant,
    total_records: Option<u64>,
    received: RangeSetBlaze<u64>,
}

impl ConnectedDevice {
    fn tick(&mut self) -> Option<Message> {
        match &self.state {
            DeviceState::Discovered => {
                self.received.clear();
                self.touch();
                self.state = DeviceState::Learning;

                Some(Message::Query)
            }
            DeviceState::Receiving(range) => {
                if !self.has_range(range) {
                    return None;
                }

                info!("have range!");

                if let Some(range) = self.requires() {
                    let reply = Message::Require(range.clone());
                    self.state = DeviceState::Receiving(range);
                    Some(reply)
                } else {
                    self.state = DeviceState::Synced;
                    None
                }
            }
            _ => None,
        }
    }

    fn handle(&mut self, message: &Message) -> Result<Option<Message>> {
        match message {
            Message::Statistics { tail } => {
                self.total_records = Some(*tail);
                self.total_records = Some(1000); // TESTING

                let range = self.requires().expect("required range expected");

                let reply = Message::Require(range.clone());

                self.state = DeviceState::Receiving(range);

                Ok(Some(reply))
            }
            Message::Records { first, records } => {
                self.received((0..records.len()).map(|v| v as u64 + *first));

                info!("{:?} progress {:.2}%", self.state, self.progress() * 100.0);

                Ok(self.tick())
            }
            _ => Ok(None),
        }
    }

    fn requires(&self) -> Option<RecordRange> {
        if self.total_records.is_none() {
            return None;
        }

        let total = self.total_records.unwrap();

        match self.received.last() {
            Some(last) => {
                if last >= total {
                    None
                } else {
                    Some(RecordRange::new(last, last + self.batch_size))
                }
            }
            _ => Some(RecordRange::new(0, self.batch_size)),
        }
    }

    fn has_range(&self, range: &RecordRange) -> bool {
        let testing = range.into_set();
        let diff = &testing - &self.received;
        if diff.len() != 0 {
            info!(
                "testing={:?} received={:?} diff={:?} {:?}",
                testing.len(),
                self.received.len(),
                diff.len(),
                diff
            );
        }
        diff.len() == 0
    }

    fn touch(&mut self) {
        self.activity = Instant::now();
    }

    fn received<I>(&mut self, records: I)
    where
        I: Iterator<Item = u64>,
    {
        let mut appending = RangeSetBlaze::from_iter(records);
        self.received.append(&mut appending);
        self.touch()
    }

    /*
    fn remaining(&self) -> RangeSetBlaze<u64> {
        if let Some(total) = self.total_records {
            let complete = RangeSetBlaze::from_iter([0..=total]);
            complete - &self.received
        } else {
            RangeSetBlaze::new()
        }
    }
    */

    fn progress(&self) -> f32 {
        if let Some(total) = self.total_records {
            let complete = RangeSetBlaze::from_iter([0..=total]);
            let p = self.received.len() as f32 / complete.len() as f32;

            // We can receive more records than we ask for and this is probably
            // better than excluding them since, well, we do have those extra
            // records haha
            if p > 1.0 {
                1.0
            } else {
                p
            }
        } else {
            0.0
        }
    }
}

#[derive(Debug)]
enum ServerCommand {
    Discovered(Discovered),
    Received(SocketAddr, Message),
    Tick,
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

        let mut device_id_by_addr: HashMap<SocketAddr, DeviceId> = HashMap::new();
        let mut devices: HashMap<DeviceId, ConnectedDevice> = HashMap::new();
        let (tx, mut rx) = mpsc::channel::<ServerCommand>(32);

        let pump = tokio::spawn({
            let mut locked = self.sender.lock().await;
            *locked = Some(tx.clone());
            async move {
                while let Some(cmd) = rx.recv().await {
                    match &cmd {
                        ServerCommand::Discovered(discovered) => {
                            info!("{:?}", cmd);

                            let entry = devices.entry(discovered.device_id.clone());
                            let entry = entry.or_insert_with(|| ConnectedDevice {
                                batch_size: 100,
                                discovered: discovered.clone(),
                                state: DeviceState::Discovered,
                                activity: Instant::now(),
                                total_records: None,
                                received: RangeSetBlaze::new(),
                            });

                            device_id_by_addr
                                .entry(discovered.addr)
                                .or_insert(discovered.device_id.clone());

                            if let Some(message) = entry.tick() {
                                transmit(&sending, &discovered.addr, &message)
                                    .await
                                    .expect("send failed");
                            }
                        }
                        ServerCommand::Received(addr, message) => {
                            info!("{:?}", message);

                            if let Some(device_id) = device_id_by_addr.get(addr) {
                                match devices.get_mut(device_id) {
                                    Some(connected) => {
                                        let reply = connected
                                            .handle(message)
                                            .expect("failed handling message");

                                        if let Some(reply) = reply {
                                            transmit(&sending, &addr, &reply)
                                                .await
                                                .expect("send failed");
                                        }
                                    }
                                    None => todo!(),
                                }
                            };
                        }
                        ServerCommand::Tick => {
                            // TODO Cheeck for stalled Receiving states.
                        }
                    }
                }
            }
        });

        let receive = tokio::spawn({
            let tx = tx.clone();

            async move {
                let mut buffer = vec![0u8; 4096];

                loop {
                    let (len, addr) = receiving
                        .recv_from(&mut buffer[..])
                        .await
                        .expect("recv failed");

                    debug!("{} bytes from {}", len, addr);

                    let message = Message::read(&buffer[0..len]).expect("parse failed");

                    tx.send(ServerCommand::Received(addr, message))
                        .await
                        .expect("send self failed");
                }
            }
        });

        let timer = tokio::spawn(async move {
            let mut interval = time::interval(Duration::from_millis(1000));

            loop {
                interval.tick().await;
                tx.send(ServerCommand::Tick)
                    .await
                    .expect("send self failed");
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
            _ = timer => {
                println!("timer done");
                Ok(())
            }
        }
    }

    pub async fn sync(&self, discovered: Discovered) -> Result<()> {
        self.send(ServerCommand::Discovered(discovered)).await
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
    Require(RecordRange),
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
                let head = reader.read_fixed32(bytes)? as u64;
                let tail = reader.read_fixed32(bytes)? as u64;
                Ok(Self::Require(RecordRange::new(head, tail)))
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
            Message::Require(range) => {
                writer.write_fixed32(2)?;
                writer.write_fixed32(range.head() as u32)?;
                writer.write_fixed32(range.tail() as u32)?;
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
