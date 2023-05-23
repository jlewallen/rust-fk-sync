use anyhow::Result;
use backoff::{backoff::Backoff, ExponentialBackoff, ExponentialBackoffBuilder};
use discovery::{DeviceId, Discovered};
use range_set_blaze::prelude::*;
use std::{
    collections::HashMap,
    net::{SocketAddr, SocketAddrV4},
    ops::RangeInclusive,
    sync::Arc,
    time::Duration,
};
use tokio::{
    net::{ToSocketAddrs, UdpSocket},
    sync::mpsc,
    sync::mpsc::Sender,
    sync::Mutex,
    time::{self, Instant},
};
use tracing::*;

const IP_ALL: [u8; 4] = [0, 0, 0, 0];
const STALLED_EXPECTING_MILLIS: u64 = 5000;
const STALLED_RECEIVING_MILLIS: u64 = 500;
const EXPIRED_MILLIS: u64 = 1000 * 60 * 2;

#[derive(Clone, Debug)]
pub enum DeviceState {
    Discovered,
    Learning,
    Receiving(RecordRange),
    Expecting((f32, Box<DeviceState>)),
    Synced,
    Failed,
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
pub struct Progress {
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
pub struct ConnectedDevice {
    device_id: DeviceId,
    addr: SocketAddr,
    batch_size: u64,
    state: DeviceState,
    received_at: Instant,
    total_records: Option<u64>,
    received: RangeSetBlaze<u64>,
    backoff: ExponentialBackoff,
    waiting_until: Option<Instant>,
    syncing_started: Option<Instant>,
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
            syncing_started: None,
        }
    }

    fn stall_backoff() -> ExponentialBackoff {
        ExponentialBackoffBuilder::new()
            .with_initial_interval(Duration::from_millis(5000))
            .with_randomization_factor(0.0)
            .with_multiplier(1.8)
            .with_max_elapsed_time(Some(Duration::from_secs(30)))
            .build()
    }

    async fn tick(&mut self, publish: &Sender<ServerEvent>) -> Result<Option<Message>> {
        match &self.state {
            DeviceState::Discovered => {
                self.received.clear();
                self.transition(DeviceState::expecting(DeviceState::Learning));

                Ok(Some(Message::Query))
            }
            DeviceState::Failed => Ok(None),
            _ => {
                if self.is_stalled() {
                    if let Some(delay) = self.backoff.next_backoff() {
                        info!(
                            "{:?} STALL {:?} {:?}",
                            self.device_id,
                            self.last_received_at(),
                            self.received_has_gaps()
                        );

                        self.waiting_until = Some(Instant::now() + delay);
                        Ok(self.query_requires())
                    } else {
                        info!(
                            "{:?} FAILED {:?} {:?}",
                            self.device_id,
                            self.last_received_at(),
                            self.received_has_gaps()
                        );

                        self.transition(DeviceState::Failed);

                        publish
                            .send(ServerEvent::Failed(self.device_id.clone()))
                            .await?;

                        Ok(None)
                    }
                } else {
                    Ok(None)
                }
            }
        }
    }

    fn handle(&mut self, message: &Message) -> Result<Option<Message>> {
        self.received_at = Instant::now();
        self.backoff.reset();
        self.waiting_until = None;

        if let DeviceState::Expecting((_deadline, after)) = &self.state {
            info!("expectation fulfilled");
            self.transition(*Box::clone(after)) // TODO into_inner on unstable
        }

        match message {
            Message::Statistics { nrecords } => {
                self.total_records = Some(*nrecords);
                self.syncing_started = Some(Instant::now());
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
            if self.total_records.is_some() {
                self.transition(DeviceState::Synced);
                let elapsed = Instant::now() - self.syncing_started.unwrap();
                info!("syncing took {:?}", elapsed);
            }
            None
        }
    }

    fn requires(&self) -> Option<RecordRange> {
        fn cap_length(r: RangeInclusive<u64>, len: u64) -> RangeInclusive<u64> {
            assert!(len > 0);
            let end = *r.end();
            let start = *r.start();
            if end - start > len {
                start..=(start + len - 1)
            } else {
                r
            }
        }

        fn try_merge(
            first: Option<RangeInclusive<u64>>,
            second: RangeInclusive<u64>,
            len: u64,
        ) -> Option<RangeInclusive<u64>> {
            match &first {
                Some(first) => {
                    let combined = *first.start()..=*second.end();
                    let width = combined.end() - combined.start();
                    if width > len {
                        Some(first.clone())
                    } else {
                        Some(combined)
                    }
                }
                None => Some(second),
            }
        }

        let total_records = self.total_records?;
        let total_required = RangeSetBlaze::from_iter([0..=total_records - 1]) - &self.received;

        let mut requiring: Option<RangeInclusive<u64>> = None;
        for range in total_required.into_ranges() {
            requiring = try_merge(requiring, cap_length(range, self.batch_size), 500);
        }

        requiring.map(RecordRange)
    }

    fn transition(&mut self, new: DeviceState) {
        info!("{:?} -> {:?}", self.state, new);
        self.state = new;
    }

    fn received(&mut self, records: RangeSetBlaze<u64>) {
        let mut appending = records;
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
            let range = 0..=(total - 1);
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
                let receiving = range.to_set();
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
pub enum ServerCommand {
    Begin(Discovered),
    Cancel(DeviceId),
    Received(SocketAddr, Message),
    Tick,
}

#[derive(Debug)]
pub enum ServerEvent {
    Began(DeviceId),
    Progress(DeviceId, Progress),
    Completed(DeviceId),
    Failed(DeviceId),
}

pub struct Server {
    port: u16,
    publish: Sender<ServerEvent>,
    sender: Arc<Mutex<Option<Sender<ServerCommand>>>>,
}

impl Server {
    pub fn new(publish: Sender<ServerEvent>) -> Self {
        Self {
            port: 22144,
            publish,
            sender: Arc::new(Mutex::new(None)),
        }
    }
    pub async fn run(&self) -> Result<()> {
        let listening_addr = SocketAddrV4::new(IP_ALL.into(), self.port);
        let receiving = Arc::new(self.bind(&listening_addr)?);
        let sending = receiving.clone();
        let publish = self.publish.clone();

        info!("listening on {}", listening_addr);

        let mut device_id_by_addr: HashMap<SocketAddr, DeviceId> = HashMap::new();
        let mut devices: HashMap<DeviceId, ConnectedDevice> = HashMap::new();
        let (tx, mut rx) = mpsc::channel::<ServerCommand>(32);

        let pump = tokio::spawn({
            let mut locked = self.sender.lock().await;
            *locked = Some(tx.clone());
            async move {
                while let Some(cmd) = rx.recv().await {
                    match handle_server_command(
                        &cmd,
                        &sending,
                        &publish,
                        &mut devices,
                        &mut device_id_by_addr,
                    )
                    .await
                    {
                        Ok(_) => {}
                        Err(e) => warn!("Server command error: {:?}", e),
                    }
                }
            }
        });

        let receive = tokio::spawn({
            let tx = tx.clone();

            async move {
                loop {
                    match receive_and_process(&receiving, &tx).await {
                        Ok(_) => {}
                        Err(e) => warn!("Receive and process error: {:?}", e),
                    }
                }
            }
        });

        let timer = tokio::spawn(async move {
            let mut interval = time::interval(Duration::from_millis(250));

            loop {
                interval.tick().await;

                match tx.send(ServerCommand::Tick).await {
                    Err(e) => warn!("Tick error: {:?}", e),
                    Ok(_) => {}
                }
            }
        });

        tokio::select! {
            _ = pump => Ok(()),
            _ = receive => Ok(()),
            _ = timer => Ok(())
        }
    }

    pub async fn sync(&self, discovered: Discovered) -> Result<()> {
        self.send(ServerCommand::Begin(discovered)).await
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

async fn handle_server_command(
    cmd: &ServerCommand,
    sending: &UdpSocket,
    publish: &Sender<ServerEvent>,
    devices: &mut HashMap<DeviceId, ConnectedDevice>,
    device_id_by_addr: &mut HashMap<SocketAddr, DeviceId>,
) -> Result<()> {
    match cmd {
        ServerCommand::Begin(discovered) => {
            let entry = devices.entry(discovered.device_id.clone());
            let entry = entry.or_insert_with(|| {
                ConnectedDevice::new(discovered.device_id.clone(), discovered.udp_addr)
            });

            device_id_by_addr
                .entry(discovered.udp_addr)
                .or_insert(discovered.device_id.clone());

            if let Some(message) = entry.tick(publish).await? {
                transmit(sending, &discovered.udp_addr, &message).await?;
                publish
                    .send(ServerEvent::Began(discovered.device_id.clone()))
                    .await?;
            }
        }
        ServerCommand::Received(addr, message) => {
            message.log_received();

            if let Some(device_id) = device_id_by_addr.get(addr) {
                match devices.get_mut(device_id) {
                    Some(connected) => {
                        let reply = connected.handle(message)?;

                        if let Some(reply) = reply {
                            transmit(sending, &addr, &reply).await?;
                        }

                        if let Some(progress) = connected.progress() {
                            publish
                                .send(ServerEvent::Progress(device_id.clone(), progress))
                                .await?;
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
                .map(|(device_id, connected)| (device_id.clone(), connected.addr))
                .collect::<Vec<_>>()
            {
                info!("{:?}@{:?} expired", device_id, addr);
                device_id_by_addr.remove(&addr);
                devices.remove(&device_id);
                // publish.send(ServerEvent::Failed(device_id.clone())).await?;
            }

            for (_, connected) in devices.iter_mut() {
                if let Some(message) = connected.tick(publish).await? {
                    transmit(&sending, &connected.addr, &message).await?;
                }
            }
        }
        ServerCommand::Cancel(device_id) => {
            if let Some(connected) = devices.remove_entry(device_id) {
                device_id_by_addr.remove(&connected.1.addr);
                publish.send(ServerEvent::Failed(device_id.clone())).await?;
            }
        }
    }

    Ok(())
}

async fn receive_and_process(receiving: &UdpSocket, tx: &Sender<ServerCommand>) -> Result<()> {
    let mut buffer = vec![0u8; 4096];

    let (len, addr) = receiving.recv_from(&mut buffer[..]).await?;

    trace!("{} bytes from {}", len, addr);

    let message = Message::read(&buffer[0..len])?;

    tx.send(ServerCommand::Received(addr, message)).await?;

    Ok(())
}

async fn transmit<A>(tx: &UdpSocket, addr: &A, m: &Message) -> Result<()>
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

#[derive(Clone, PartialEq, Debug)]
pub struct RecordRange(RangeInclusive<u64>);

impl RecordRange {
    fn new(h: u64, t: u64) -> Self {
        Self(RangeInclusive::new(h, t))
    }

    fn head(&self) -> u64 {
        *self.0.start()
    }

    fn tail(&self) -> u64 {
        *self.0.end()
    }

    fn to_set(&self) -> RangeSetBlaze<u64> {
        RangeSetBlaze::from_iter([self.head()..=self.tail()])
    }
}

impl From<RecordRange> for RangeInclusive<u64> {
    fn from(val: RecordRange) -> Self {
        val.0
    }
}

impl From<&RangeInclusive<u64>> for RecordRange {
    fn from(value: &RangeInclusive<u64>) -> Self {
        RecordRange(value.clone())
    }
}

pub struct RawRecord(Vec<u8>);

impl std::fmt::Debug for RawRecord {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("RawRecord").field(&self.0.len()).finish()
    }
}

#[derive(Debug)]
pub enum Message {
    Query,
    Statistics {
        nrecords: u64,
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
                let nrecords = reader.read_fixed32(bytes)? as u64;
                Ok(Self::Statistics { nrecords })
            }
            FK_UDP_PROTOCOL_KIND_REQUIRE => {
                let head = reader.read_fixed32(bytes)? as u64;
                let nrecords = reader.read_fixed32(bytes)? as u64;
                Ok(Self::Require(RecordRange::new(head, nrecords - 1)))
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
            Message::Statistics { nrecords } => {
                writer.write_fixed32(1)?;
                writer.write_fixed32(*nrecords as u32)?;
                Ok(())
            }
            Message::Require(range) => {
                let nrecords = range.tail() - range.head() + 1;
                writer.write_fixed32(2)?;
                writer.write_fixed32(range.head() as u32)?;
                writer.write_fixed32(nrecords as u32)?;
                Ok(())
            }
            Message::Records {
                head,
                flags,
                records,
            } => {
                writer.write_fixed32(3)?;
                writer.write_fixed32(*head as u32)?;
                writer.write_fixed32(*flags)?;
                assert!(records.is_empty()); // Laziness
                Ok(())
            }
            Message::Batch { flags } => {
                writer.write_fixed32(*flags)?;
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
        assert_eq!(connected.requires(), Some(RecordRange(0..=9_999)));
    }

    #[test]
    pub fn test_requires_when_some_received() {
        let mut connected = test_device();
        connected.total_records = Some(100_000);
        connected.received(RangeSetBlaze::from_iter([0..=1000]));
        assert_eq!(connected.requires(), Some(RecordRange(1001..=11_000)));
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
    pub fn test_requires_duplicates_when_gaps_are_clumped() {
        let mut connected = test_device();
        connected.total_records = Some(100_000);
        connected.received(RangeSetBlaze::from_iter([0..=1000]));
        connected.received(RangeSetBlaze::from_iter([1010..=1020]));
        connected.received(RangeSetBlaze::from_iter([1080..=1099]));
        connected.received(RangeSetBlaze::from_iter([1180..=2000]));
        assert_eq!(connected.requires(), Some(RecordRange(1001..=1179)));
    }

    #[test]
    pub fn test_requires_with_gap_wider_than_batch_size() {
        let mut connected = test_device();
        connected.total_records = Some(100_000);
        connected.received(RangeSetBlaze::from_iter([0..=1000]));
        connected.received(RangeSetBlaze::from_iter([30_000..=40_000]));
        assert_eq!(connected.requires(), Some(RecordRange(1001..=11_000)));
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
