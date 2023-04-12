use anyhow::Result;
use range_set_blaze::prelude::*;
use std::{
    collections::HashMap,
    net::{Ipv4Addr, SocketAddr, SocketAddrV4},
    ops::RangeInclusive,
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

const STALL_SECONDS: u64 = 5;

#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub struct DeviceId(String);

#[derive(Clone, Debug)]
pub struct Discovered {
    device_id: DeviceId,
    addr: SocketAddr,
}

#[derive(Debug, Clone)]
struct RecordRange((u64, u64));

impl RecordRange {
    fn new(h: u64, t: u64) -> Self {
        Self((h, t))
    }

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

impl From<&RangeInclusive<u64>> for RecordRange {
    fn from(value: &RangeInclusive<u64>) -> Self {
        RecordRange((*value.start(), *value.end() + 1))
    }
}

#[derive(Debug)]
enum DeviceState {
    Discovered,
    Learning,
    Receiving(RecordRange),
    Expecting((Instant, Box<DeviceState>)),
    Synced,
}

struct BatchProgress {
    completed: f32,
    total: usize,
    received: usize,
}

impl std::fmt::Debug for BatchProgress {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_fmt(format_args!(
            "{:.2} ({}/{})",
            self.completed, self.received, self.total
        ))
    }
}

#[allow(dead_code)]
struct Progress {
    last_activity: Duration,
    total: Option<BatchProgress>,
    batch: Option<BatchProgress>,
}

impl std::fmt::Debug for Progress {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match (&self.total, &self.batch) {
            (Some(total), Some(batch)) => {
                f.write_fmt(format_args!("B({:?}) T({:?})", batch, total))
            }
            (None, None) => f.write_fmt(format_args!("Activity={:?}", &self.last_activity)),
            (_, _) => todo!("Nonsensical progress!"),
        }
    }
}

impl DeviceState {}

#[derive(Debug)]
struct ConnectedDevice {
    batch_size: u64,
    #[allow(dead_code)]
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
            DeviceState::Expecting((deadline, after)) => {
                todo!()
            }
            DeviceState::Receiving(range) => {
                if !self.has_range(range) {
                    return None;
                }

                if let Some(range) = self.requires() {
                    info!("{:?} received, requiring {:?}", range, range);
                    let reply = Message::Require(range.clone());
                    self.state = DeviceState::Receiving(range);
                    Some(reply)
                } else {
                    info!("{:?} received, synced", range);
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
                if let Some(range) = self.requires() {
                    let reply = Message::Require(range.clone());
                    self.state = DeviceState::Receiving(range);
                    Ok(Some(reply))
                } else {
                    Ok(None)
                }
            }
            Message::Records {
                head,
                flags: _flags,
                records,
            } => {
                // Include the received records in our received set.
                let just_received: Vec<u64> =
                    (0..records.len()).map(|v| v as u64 + *head).collect();
                self.received(just_received.into_iter());

                info!(
                    "{:?} {:?} {:?}",
                    self.state,
                    self.progress(),
                    &self.received_has_gaps()
                );

                Ok(self.tick())
            }
            Message::Batch { flags: _flags } => Ok(self.fill_gaps()),
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
                let last = last + 1;
                if last < total {
                    Some(RecordRange::new(
                        last,
                        std::cmp::min(last + self.batch_size, total),
                    ))
                } else {
                    None
                }
            }
            _ => Some(RecordRange::new(0, std::cmp::min(self.batch_size, total))),
        }
    }

    fn has_range(&self, range: &RecordRange) -> bool {
        let testing = range.into_set();
        let diff = &testing - &self.received;
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

    fn completed(&self) -> Option<BatchProgress> {
        if let Some(total) = self.total_records {
            let complete = RangeSetBlaze::from_iter([0..=total]);
            let total = complete.len() as usize;
            let received = self.received.len() as usize;
            let completed = received as f32 / total as f32;
            // We can receive more records than we ask for and this is probably
            // better than excluding them since, well, we do have those extra
            // records haha
            let completed = if completed > 1.0 { 1.0 } else { completed };

            Some(BatchProgress {
                completed,
                total,
                received,
            })
        } else {
            None
        }
    }

    fn batch(&self) -> Option<BatchProgress> {
        match &self.state {
            DeviceState::Receiving(range) => {
                let receiving = range.into_set();
                let remaining = &receiving - &self.received;
                let total = receiving.len() as usize;
                let received = total - remaining.len() as usize;
                let completed = received as f32 / total as f32;

                Some(BatchProgress {
                    completed,
                    total,
                    received,
                })
            }
            _ => None,
        }
    }

    fn progress(&self) -> Option<Progress> {
        match &self.state {
            DeviceState::Receiving(_range) => Some(Progress {
                last_activity: self.last_activity(),
                total: self.completed(),
                batch: self.batch(),
            }),
            _ => None,
        }
    }

    fn last_activity(&self) -> Duration {
        Instant::now() - self.activity
    }

    fn is_stalled(&self) -> bool {
        match &self.state {
            DeviceState::Receiving(_) => self.last_activity() > Duration::from_secs(STALL_SECONDS),
            _ => false,
        }
    }

    fn received_has_gaps(&self) -> HasGaps {
        match self.received.last() {
            Some(last) => {
                if last as u128 + 1 == self.received.len() {
                    HasGaps::Continuous((0, last))
                } else {
                    let inverted = RangeSetBlaze::from_iter([0..=last]) - &self.received;
                    let gaps = inverted
                        .into_ranges()
                        .collect::<Vec<std::ops::RangeInclusive<_>>>();
                    HasGaps::HasGaps(gaps)
                }
            }
            None => HasGaps::Continuous((0, 0)),
        }
    }

    fn fill_gaps(&mut self) -> Option<Message> {
        match self.received_has_gaps() {
            HasGaps::Continuous(_) => None,
            HasGaps::HasGaps(gaps) => {
                info!("fill-gaps!");
                Some(Message::Require(gaps.get(0).unwrap().into()))
            }
        }
    }

    fn recover(&mut self) -> Option<Message> {
        info!("recover!");

        match self.received_has_gaps() {
            HasGaps::Continuous(_) => self.retry(),
            HasGaps::HasGaps(_) => self.fill_gaps(),
        }
    }

    fn retry(&mut self) -> Option<Message> {
        info!("retry!");

        match &self.state {
            DeviceState::Receiving(range) => Some(Message::Require(range.clone())),
            _ => None,
        }
    }
}

#[derive(Debug)]
enum HasGaps {
    Continuous((u64, u64)),
    HasGaps(Vec<RangeInclusive<u64>>),
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
        let listening_addr = SocketAddrV4::new(IP_ALL.into(), self.port);
        let receiving = Arc::new(self.bind(&listening_addr)?);
        let sending = receiving.clone();

        info!("listening on {}", listening_addr);

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
                            debug!("{:?}", cmd);

                            let entry = devices.entry(discovered.device_id.clone());
                            let entry = entry.or_insert_with(|| ConnectedDevice {
                                batch_size: 10000,
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
                            message.log_received();

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
                            for (device_id, connected) in devices.iter_mut() {
                                if connected.is_stalled() {
                                    let last_activity = connected.last_activity();
                                    info!(
                                        "{:?} stalled {:?} {:?}",
                                        device_id,
                                        &last_activity,
                                        &connected.received_has_gaps()
                                    );

                                    if let Some(reply) = connected.recover() {
                                        let addr = &connected.discovered.addr;
                                        transmit(&sending, addr, &reply)
                                            .await
                                            .expect("send failed");
                                    }
                                }
                            }
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

                    trace!("{} bytes from {}", len, addr);

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

    async fn sync(&self, discovered: Discovered) -> Result<()> {
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
            trace!("{} bytes from {}", len, addr);

            let bytes = &buffer[0..len];
            let announced = Announce::parse(bytes)?;
            let discovered = Discovered {
                device_id: announced.device_id().clone(),
                addr: {
                    let mut addr = addr;
                    addr.set_port(22144);
                    addr
                },
            };

            trace!("discovered {:?}", discovered);

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
    Statistics {
        tail: u64,
    },
    Require(RecordRange),
    Records {
        head: u64,
        flags: u32,
        records: Vec<RawRecord>,
    },
    Batch {
        flags: u32,
    },
}

const FK_UDP_PROTOCOL_KIND_QUERY: u32 = 0;
const FK_UDP_PROTOCOL_KIND_STATISTICS: u32 = 1;
const FK_UDP_PROTOCOL_KIND_REQUIRE: u32 = 2;
const FK_UDP_PROTOCOL_KIND_RECORDS: u32 = 3;
const FK_UDP_PROTOCOL_KIND_BATCH: u32 = 4;

impl Message {
    fn read(bytes: &[u8]) -> Result<Self> {
        use quick_protobuf::reader::BytesReader;

        let mut reader = BytesReader::from_bytes(bytes);
        let kind = reader.read_fixed32(bytes)?;

        match kind {
            FK_UDP_PROTOCOL_KIND_QUERY => Ok(Self::Query {}),
            FK_UDP_PROTOCOL_KIND_STATISTICS => {
                let tail = reader.read_fixed32(bytes)? as u64;
                Ok(Self::Statistics { tail })
            }
            FK_UDP_PROTOCOL_KIND_REQUIRE => {
                let head = reader.read_fixed32(bytes)? as u64;
                let tail = reader.read_fixed32(bytes)? as u64;
                Ok(Self::Require(RecordRange::new(head, tail)))
            }
            FK_UDP_PROTOCOL_KIND_RECORDS => {
                let head = reader.read_fixed32(bytes)? as u64;
                let flags = reader.read_fixed32(bytes)?;
                let mut records: Vec<RawRecord> = Vec::new();
                while !reader.is_eof() {
                    let record = reader.read_bytes(bytes)?;
                    records.push(RawRecord(record.into()));
                }

                Ok(Self::Records {
                    head,
                    flags,
                    records,
                })
            }
            FK_UDP_PROTOCOL_KIND_BATCH => {
                let flags = reader.read_fixed32(bytes)?;

                Ok(Self::Batch { flags })
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
            Message::Records {
                head,
                flags,
                records,
            } => {
                writer.write_fixed32(3)?;
                writer.write_fixed32(*head as u32)?;
                writer.write_fixed32(*flags as u32)?;
                assert!(records.len() == 0); // Laziness
                Ok(())
            }
            Message::Batch { flags } => {
                writer.write_fixed32(*flags as u32)?;
                Ok(())
            }
        }
    }

    fn log_received(&self) {
        match self {
            Message::Records {
                head: _head,
                flags: _flags,
                records: _records,
            } => trace!("{:?}", self),
            _ => debug!("{:?}", self),
        }
    }
}

enum Announce {
    Hello(DeviceId),
    Bye(DeviceId),
}

impl Announce {
    fn parse(bytes: &[u8]) -> Result<Self> {
        use quick_protobuf::BytesReader;

        let mut reader = BytesReader::from_bytes(bytes);
        let size = reader.read_varint32(bytes)?;
        let tag = reader.next_tag(bytes)?;
        assert_eq!(tag >> 3, 1);
        let id_bytes = reader.read_bytes(bytes)?;
        assert_eq!(id_bytes.len(), 16);
        let device_id = DeviceId(hex::encode(id_bytes));

        if size == 18 {
            Ok(Announce::Hello(device_id))
        } else {
            Ok(Announce::Bye(device_id))
        }
    }

    fn device_id(&self) -> &DeviceId {
        match self {
            Announce::Hello(id) => id,
            Announce::Bye(id) => id,
        }
    }
}
