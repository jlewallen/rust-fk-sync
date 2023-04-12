use anyhow::Result;
use backoff::{backoff::Backoff, ExponentialBackoff, ExponentialBackoffBuilder};
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

const IP_ALL: [u8; 4] = [0, 0, 0, 0];
const STALLED_EXPECTING_MILLIS: u64 = 5000;
const STALLED_RECEIVING_MILLIS: u64 = 500;
const EXPIRED_MILLIS: u64 = 1000 * 60 * 2;

#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub struct DeviceId(String);

#[derive(Clone, Debug)]
pub struct Discovered {
    device_id: DeviceId,
    addr: SocketAddr,
}

#[derive(Clone, Debug)]
enum DeviceState {
    Discovered,
    Learning,
    Receiving(RecordRange),
    Expecting((f32, Box<DeviceState>)),
    Synced,
}

impl DeviceState {
    fn expecting(after: Self) -> Self {
        Self::Expecting((0.0, Box::new(after)))
    }
}

struct RangeProgress {
    range: RangeInclusive<u64>,
    completed: f32,
    total: usize,
    received: usize,
}

impl std::fmt::Debug for RangeProgress {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_fmt(format_args!(
            "{:.3} ({}/{}, {:?})",
            self.completed, self.received, self.total, self.range
        ))
    }
}

#[allow(dead_code)]
struct Progress {
    total: Option<RangeProgress>,
    batch: Option<RangeProgress>,
}

impl std::fmt::Debug for Progress {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match (&self.total, &self.batch) {
            (Some(total), Some(batch)) => {
                f.write_fmt(format_args!("T({:?}) B({:?})", total, batch))
            }
            (None, None) => f.write_str("Idle"),
            (_, _) => todo!("Nonsensical progress!"),
        }
    }
}

#[derive(Debug)]
struct ConnectedDevice {
    device_id: DeviceId,
    addr: SocketAddr,
    batch_size: u64,
    state: DeviceState,
    received_at: Instant,
    total_records: Option<u64>,
    received: RangeSetBlaze<u64>,
    backoff: ExponentialBackoff,
    waiting_until: Option<Instant>,
}

impl ConnectedDevice {
    fn new(device_id: DeviceId, addr: SocketAddr) -> Self {
        Self {
            device_id,
            addr,
            batch_size: 10000,
            state: DeviceState::Discovered,
            received_at: Instant::now(),
            total_records: None,
            received: RangeSetBlaze::new(),
            backoff: Self::stall_backoff(),
            waiting_until: None,
        }
    }

    fn stall_backoff() -> ExponentialBackoff {
        ExponentialBackoffBuilder::new()
            .with_initial_interval(Duration::from_millis(5000))
            .with_randomization_factor(0.0)
            .with_multiplier(1.8)
            .build()
    }

    fn tick(&mut self) -> Option<Message> {
        match &self.state {
            DeviceState::Discovered => {
                self.received.clear();
                self.transition(DeviceState::expecting(DeviceState::Learning));

                Some(Message::Query)
            }
            _ => {
                if self.is_stalled() {
                    info!(
                        "{:?} STALL {:?} {:?}",
                        self.device_id,
                        self.last_received_at(),
                        self.received_has_gaps()
                    );

                    if let Some(delay) = self.backoff.next_backoff() {
                        self.waiting_until = Some(Instant::now() + delay);
                        self.query_requires()
                    } else {
                        None
                    }
                } else {
                    None
                }
            }
        }
    }

    fn handle(&mut self, message: &Message) -> Result<Option<Message>> {
        self.received_at = Instant::now();
        self.backoff.reset();
        self.waiting_until = None;

        match &self.state {
            DeviceState::Expecting((_deadline, after)) => {
                info!("expectation fulfilled");
                self.transition(*Box::clone(&after)) // TODO into_inner on unstable
            }
            _ => {}
        }

        match message {
            Message::Statistics { tail } => {
                self.total_records = Some(*tail);
                Ok(self.query_requires())
            }
            Message::Records {
                head,
                flags: _flags,
                records,
            } => {
                // Include the received records in our received set.
                let just_received: RangeSetBlaze<u64> =
                    (0..records.len()).map(|v| v as u64 + *head).collect();
                self.received(just_received);

                let progress = self.progress();
                let has_gaps = self.received_has_gaps();
                let progress = progress.map_or("".to_owned(), |f| format!("{:?}", f));
                info!("{} {:?}", progress, &has_gaps);

                Ok(None)
            }
            Message::Batch { flags: _flags } => Ok(self.query_requires()),
            _ => Ok(None),
        }
    }

    fn query_requires(&mut self) -> Option<Message> {
        if let Some(range) = self.requires() {
            self.transition(DeviceState::expecting(DeviceState::Receiving(
                range.clone(),
            )));
            Some(Message::Require(range))
        } else {
            if !self.total_records.is_none() {
                self.transition(DeviceState::Synced);
            }
            None
        }
    }

    fn requires(&self) -> Option<RecordRange> {
        if self.total_records.is_none() {
            return None;
        }

        fn cap_length(r: RangeInclusive<u64>, len: u64) -> RangeInclusive<u64> {
            let end = *r.end();
            let start = *r.start();
            if end - start > len {
                start..=(start + len)
            } else {
                r
            }
        }

        let total_records = self.total_records.unwrap();
        let total_required = RangeSetBlaze::from_iter([0..=total_records]) - &self.received;

        for range in total_required.into_ranges() {
            return Some(RecordRange::from_range(cap_length(range, self.batch_size)));
        }

        None
    }

    fn transition(&mut self, new: DeviceState) {
        info!("{:?} -> {:?}", self.state, new);
        self.state = new;
    }

    fn received(&mut self, records: RangeSetBlaze<u64>) {
        let mut appending = records.clone();
        self.received.append(&mut appending);
    }

    fn last_received_at(&self) -> Duration {
        Instant::now() - self.received_at
    }

    fn is_expired(&self) -> bool {
        match &self.state {
            DeviceState::Receiving(_) => {
                self.last_received_at() > Duration::from_millis(EXPIRED_MILLIS)
            }
            _ => false,
        }
    }

    fn is_stalled(&self) -> bool {
        if let Some(waiting_until) = self.waiting_until {
            if Instant::now() < waiting_until {
                return false;
            }
        }

        match &self.state {
            DeviceState::Expecting(_) => {
                self.last_received_at() > Duration::from_millis(STALLED_EXPECTING_MILLIS)
            }
            DeviceState::Receiving(_) => {
                self.last_received_at() > Duration::from_millis(STALLED_RECEIVING_MILLIS)
            }
            _ => false,
        }
    }

    fn received_has_gaps(&self) -> HasGaps {
        match self.received.last() {
            Some(last) => {
                let full_range = 0..=last;
                if last as u128 + 1 == self.received.len() {
                    HasGaps::Continuous(full_range)
                } else {
                    let inverted = RangeSetBlaze::from_iter(full_range.clone()) - &self.received;
                    let gaps = inverted
                        .into_ranges()
                        .collect::<Vec<std::ops::RangeInclusive<_>>>();
                    HasGaps::HasGaps(full_range, gaps)
                }
            }
            None => HasGaps::Continuous(0..=0),
        }
    }

    fn completed(&self) -> Option<RangeProgress> {
        if let Some(total) = self.total_records {
            let range = 0..=total;
            let complete = RangeSetBlaze::from_iter([range.clone()]);
            let total = complete.len() as usize;
            let received = self.received.len() as usize;
            let completed = received as f32 / total as f32;
            // We can receive more records than we ask for and this is probably
            // better than excluding them since, well, we do have those extra
            // records haha
            let completed = if completed > 1.0 { 1.0 } else { completed };

            Some(RangeProgress {
                range,
                completed,
                total,
                received,
            })
        } else {
            None
        }
    }

    fn batch(&self) -> Option<RangeProgress> {
        match &self.state {
            DeviceState::Receiving(range) => {
                let receiving = range.into_set();
                let remaining = &receiving - &self.received;
                let total = receiving.len() as usize;
                let received = total - remaining.len() as usize;
                let completed = received as f32 / total as f32;

                Some(RangeProgress {
                    range: range.clone().into(),
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
                total: self.completed(),
                batch: self.batch(),
            }),
            _ => None,
        }
    }
}

#[derive(Debug)]
enum HasGaps {
    Continuous(RangeInclusive<u64>),
    HasGaps(RangeInclusive<u64>, Vec<RangeInclusive<u64>>),
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
                            let entry = devices.entry(discovered.device_id.clone());
                            let entry = entry.or_insert_with(|| {
                                ConnectedDevice::new(discovered.device_id.clone(), discovered.addr)
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
                            for (device_id, addr) in devices
                                .iter()
                                .filter(|(_, connected)| connected.is_expired())
                                .map(|(device_id, connected)| {
                                    (device_id.clone(), connected.addr.clone())
                                })
                                .collect::<Vec<_>>()
                            {
                                info!("{:?}@{:?} expired", device_id, addr);
                                device_id_by_addr.remove(&addr);
                                devices.remove(&device_id);
                            }

                            for (_, connected) in devices.iter_mut() {
                                if let Some(message) = connected.tick() {
                                    transmit(&sending, &connected.addr, &message)
                                        .await
                                        .expect("send failed");
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
            let mut interval = time::interval(Duration::from_millis(250));

            loop {
                interval.tick().await;

                tx.send(ServerCommand::Tick)
                    .await
                    .expect("send self failed");
            }
        });

        tokio::select! {
            _ = pump => Ok(()),
            _ = receive => Ok(()),
            _ = timer => Ok(())
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

#[derive(Clone, PartialEq, Debug)]
struct RecordRange(RangeInclusive<u64>);

impl RecordRange {
    fn new(h: u64, t: u64) -> Self {
        Self(RangeInclusive::new(h, t))
    }

    fn from_range(range: RangeInclusive<u64>) -> Self {
        Self(range)
    }

    fn head(&self) -> u64 {
        *self.0.start()
    }

    fn tail(&self) -> u64 {
        *self.0.end()
    }

    fn into_set(&self) -> RangeSetBlaze<u64> {
        RangeSetBlaze::from_iter([self.head()..=self.tail() - 1])
    }
}

impl Into<RangeInclusive<u64>> for RecordRange {
    fn into(self) -> RangeInclusive<u64> {
        self.0.clone()
    }
}

impl From<&RangeInclusive<u64>> for RecordRange {
    fn from(value: &RangeInclusive<u64>) -> Self {
        RecordRange(value.clone())
    }
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
                assert!(tail > head);
                Ok(Self::Require(RecordRange::new(head, tail - 1)))
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
                writer.write_fixed32(range.tail() as u32 + 1)?;
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
            _ => info!("{:?}", self),
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

#[cfg(test)]
mod tests {
    use super::*;

    fn test_device() -> ConnectedDevice {
        ConnectedDevice::new(
            DeviceId("test-device".to_owned()),
            SocketAddrV4::new(IP_ALL.into(), 22144).into(),
        )
    }

    #[test]
    pub fn test_unknown_total_records() {
        let connected = test_device();
        assert_eq!(connected.requires(), None);
    }

    #[test]
    pub fn test_requires_when_none_received() {
        let mut connected = test_device();
        connected.total_records = Some(100_000);
        assert_eq!(connected.requires(), Some(RecordRange(0..=10_000)));
    }

    #[test]
    pub fn test_requires_when_some_received() {
        let mut connected = test_device();
        connected.total_records = Some(100_000);
        connected.received(RangeSetBlaze::from_iter([0..=1000]));
        assert_eq!(connected.requires(), Some(RecordRange(1001..=11_001)));
    }

    #[test]
    pub fn test_requires_with_gap() {
        let mut connected = test_device();
        connected.total_records = Some(100_000);
        connected.received(RangeSetBlaze::from_iter([0..=1000]));
        connected.received(RangeSetBlaze::from_iter([3000..=4000]));
        assert_eq!(connected.requires(), Some(RecordRange(1001..=2999)));
    }

    #[test]
    pub fn test_requires_with_gap_wider_than_batch_size() {
        let mut connected = test_device();
        connected.total_records = Some(100_000);
        connected.received(RangeSetBlaze::from_iter([0..=1000]));
        connected.received(RangeSetBlaze::from_iter([30_000..=40_000]));
        assert_eq!(connected.requires(), Some(RecordRange(1001..=11_001)));
    }

    #[test]
    pub fn test_requires_when_have_everything() {
        let mut connected = test_device();
        connected.total_records = Some(100_000);
        connected.received(RangeSetBlaze::from_iter([0..=100_000]));
        assert_eq!(connected.requires(), None);
    }

    #[test]
    pub fn test_backoff_is_sensibly_tuned() {
        let mut backoff = ConnectedDevice::stall_backoff();
        assert_eq!(backoff.next_backoff(), Some(Duration::from_millis(5000)));
        assert_eq!(backoff.next_backoff(), Some(Duration::from_millis(9000)));
    }
}
