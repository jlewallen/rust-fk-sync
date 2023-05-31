use anyhow::{anyhow, Result};
use async_trait::async_trait;
use backoff::{backoff::Backoff, ExponentialBackoff, ExponentialBackoffBuilder};
use discovery::{DeviceId, Discovered};
use range_set_blaze::prelude::*;
use std::{
    collections::HashMap,
    net::{SocketAddr, SocketAddrV4},
    ops::{RangeInclusive, Sub},
    sync::Arc,
    time::{Duration, SystemTime},
};
use tokio::{
    net::UdpSocket,
    sync::mpsc::Sender,
    sync::mpsc::{self},
    sync::Mutex,
    time::{self, Instant},
};
use tracing::*;

use crate::{
    progress::{Progress, RangeProgress},
    proto::{Message, MessageCodec, NumberedRecord, RecordRange},
};

const IP_ALL: [u8; 4] = [0, 0, 0, 0];
const STALLED_EXPECTING_MILLIS: u64 = 5000;
const STALLED_RECEIVING_MILLIS: u64 = 500;
const EXPIRED_MILLIS: u64 = 1000 * 60 * 2;
const DEFAULT_PORT: u16 = 22144;

#[derive(Debug)]
pub enum ServerEvent {
    Began(DeviceId),
    Progress(DeviceId, SystemTime, Progress),
    Completed(DeviceId),
    Failed(DeviceId),
}

#[derive(Debug)]
pub enum ServerCommand {
    Begin(Discovered),
    Cancel(DeviceId),
    Received(TransportMessage),
    Tick,
}

impl ServerCommand {
    pub fn name(&self) -> &str {
        match self {
            ServerCommand::Begin(_) => "begin",
            ServerCommand::Cancel(_) => "cancel",
            ServerCommand::Received(_) => "received",
            ServerCommand::Tick => "tick",
        }
    }
}

#[derive(Clone, Debug)]
enum DeviceState {
    Discovered,
    Learning,
    Required(RecordRange),
    Receiving(RecordRange),
    Synced,
    Failed,
}

#[derive(Debug)]
enum HasGaps {
    Continuous(RangeInclusive<u64>),
    HasGaps(RangeInclusive<u64>, Vec<RangeInclusive<u64>>),
}

#[derive(Default)]
pub struct Statistics {
    pub messages_received: u32,
}

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
    syncing_started: Option<SystemTime>,
    progress_published: Option<Instant>,
    statistics: Statistics,
}

#[derive(Debug)]
#[allow(dead_code)]
enum Transition {
    None,
    Direct(DeviceState),
    SendOnly(Message),
    Send(Message, DeviceState),
}

impl ConnectedDevice {
    fn new(device_id: DeviceId, addr: SocketAddr) -> Self {
        Self {
            device_id,
            addr,
            batch_size: 5000,
            state: DeviceState::Discovered,
            received_at: Instant::now(),
            total_records: None,
            received: RangeSetBlaze::new(),
            backoff: Self::stall_backoff(),
            waiting_until: None,
            syncing_started: None,
            progress_published: None,
            statistics: Default::default(),
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

    fn tick(&mut self) -> Result<Transition> {
        match &self.state {
            DeviceState::Synced | DeviceState::Failed => Ok(Transition::None),
            DeviceState::Discovered => {
                self.received.clear();

                Ok(Transition::Send(Message::Query, DeviceState::Learning))
            }
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

                        self.query_requires()
                    } else {
                        info!(
                            "{:?} FAILED {:?} {:?}",
                            self.device_id,
                            self.last_received_at(),
                            self.received_has_gaps()
                        );

                        Ok(Transition::Direct(DeviceState::Failed))
                    }
                } else {
                    Ok(Transition::None)
                }
            }
        }
    }

    fn handle(&mut self, message: &Message) -> Result<Transition> {
        self.received_at = Instant::now();
        self.backoff.reset();
        self.waiting_until = None;
        self.statistics.messages_received += 1;

        match message {
            Message::Statistics { nrecords } => {
                self.total_records = Some(*nrecords);
                self.syncing_started = Some(SystemTime::now());
                self.query_requires()
            }
            Message::Records {
                head,
                flags: _flags,
                sequence: _sequence,
                records,
            } => {
                assert!(!records.is_empty());

                // Include the received records in our received set.
                let just_received: RangeSetBlaze<u64> =
                    (0..records.len()).map(|v| v as u64 + *head).collect();
                self.received(just_received);

                let progress = self.progress();
                let has_gaps = self.received_has_gaps();
                let progress = progress.map_or("".to_owned(), |f| format!("{:?}", f));
                trace!(target: "transfer-progress", "{} {:?}", progress, &has_gaps);

                match &self.state {
                    DeviceState::Required(range) => {
                        Ok(Transition::Direct(DeviceState::Receiving(range.clone())))
                    }
                    _ => Ok(Transition::None),
                }
            }
            Message::Batch { flags: _flags } => self.query_requires(),
            _ => Ok(Transition::None),
        }
    }

    fn query_requires(&mut self) -> Result<Transition> {
        if let Some(range) = self.requires() {
            let new_state = DeviceState::Required(range.clone());
            Ok(Transition::Send(Message::Require(range), new_state))
        } else {
            if self.total_records.is_some() {
                let elapsed = SystemTime::now().duration_since(self.syncing_started.unwrap())?;
                info!("Syncing took {:?}", elapsed);

                Ok(Transition::Direct(DeviceState::Synced))
            } else {
                Ok(Transition::None)
            }
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
            DeviceState::Learning | DeviceState::Required(_) => {
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
            Some(RangeProgress::new(0..=(total - 1), &self.received))
        } else {
            None
        }
    }

    fn batch(&self) -> Option<RangeProgress> {
        match &self.state {
            DeviceState::Receiving(range) => {
                Some(RangeProgress::new(range.0.clone(), &self.received))
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

    async fn transition(&mut self, new: DeviceState, publish: &Sender<ServerEvent>) -> Result<()> {
        info!(
            "{:?} -> {:?} (publish:capacity = {})",
            &self.state,
            &new,
            publish.capacity()
        );

        match (&self.state, &new) {
            (DeviceState::Synced, _) => {}
            (DeviceState::Failed, _) => {}
            (_, DeviceState::Learning) => {
                publish
                    .send(ServerEvent::Began(self.device_id.clone()))
                    .await?;
            }
            (_, DeviceState::Synced) => {
                publish
                    .send(ServerEvent::Completed(self.device_id.clone()))
                    .await?;
            }
            (_, DeviceState::Failed) => {
                publish
                    .send(ServerEvent::Failed(self.device_id.clone()))
                    .await?;
            }
            (_, _) => {}
        }

        self.state = new;

        Ok(())
    }

    async fn apply(
        &mut self,
        transition: Transition,
        publish: &Sender<ServerEvent>,
        sender: &Sender<TransportMessage>,
    ) -> Result<()> {
        match transition {
            Transition::None => Ok(()),
            Transition::Direct(state) => {
                self.transition(state, publish).await?;

                Ok(())
            }
            Transition::SendOnly(sending) => {
                transmit(sender, &self.addr, sending).await?;

                Ok(())
            }
            Transition::Send(sending, state) => {
                transmit(sender, &self.addr, sending).await?;

                self.transition(state, publish).await?;

                Ok(())
            }
        }
    }
}

pub struct ReceivedRecords {
    pub device_id: DeviceId,
    pub records: Vec<NumberedRecord>,
}

impl ReceivedRecords {
    // Note that ReceivedRecords are always sequential, as they're constructed
    // from incoming packets.
    pub fn range(&self) -> Option<RangeInclusive<u64>> {
        let numbers = self.records.iter().map(|r| r.number);
        let first = numbers.clone().min();
        let last = numbers.max();
        match (first, last) {
            (Some(first), Some(last)) => Some(first..=last),
            _ => None,
        }
    }

    pub fn iter(&self) -> impl Iterator<Item = &NumberedRecord> {
        self.records.iter()
    }
}

pub trait RecordsSink: Send + Sync {
    fn write(&self, records: &ReceivedRecords) -> Result<()>;
}

#[derive(Default)]
pub struct DevNullSink {}

impl RecordsSink for DevNullSink {
    fn write(&self, _records: &ReceivedRecords) -> Result<()> {
        Ok(())
    }
}

#[derive(Debug)]
pub struct TransportMessage((SocketAddr, Message));

#[async_trait]
pub trait Transport {
    async fn open(&self, received: Sender<Vec<ServerCommand>>) -> Result<Sender<TransportMessage>>;
}

pub struct UdpTransport {
    port: u16,
}

impl UdpTransport {
    pub fn new() -> Self {
        Self { port: DEFAULT_PORT }
    }
}

#[async_trait]
impl Transport for UdpTransport {
    async fn open(&self, received: Sender<Vec<ServerCommand>>) -> Result<Sender<TransportMessage>> {
        let listening_addr = SocketAddrV4::new(IP_ALL.into(), self.port);
        let receiving = Arc::new(bind(&listening_addr)?);
        let sending = receiving.clone();

        info!("listening on {}", listening_addr);

        // This were chosen arbitrarily.
        let (send_tx, mut send_rx) = mpsc::channel::<TransportMessage>(32);

        // No amount of investigation into graceful exiting has been done.
        tokio::spawn(async move {
            let mut last_packet = None;

            loop {
                let mut codec = MessageCodec::default();
                let mut batch: Vec<ServerCommand> = Vec::new();

                receiving.readable().await.expect("Oops");

                if let Some(last_packet) = last_packet {
                    let elapsed = Instant::now().sub(last_packet);
                    if elapsed > Duration::from_millis(500) {
                        info!("last packet {:?}", elapsed);
                    }
                }
                last_packet = Some(Instant::now());

                loop {
                    let mut buffer = vec![0u8; 4096];

                    match receiving.try_recv_from(&mut buffer[..]) {
                        Ok((len, addr)) => {
                            trace!("{:?} Received {:?}", addr, len);

                            match codec.try_read(&buffer[..len]) {
                                Ok(Some(message)) => {
                                    if let Message::Batch { flags: _flags } = message {
                                        info!(
                                            "{:?} Batch recv-queue = {}",
                                            addr,
                                            received.max_capacity() - received.capacity()
                                        )
                                    }

                                    if received.capacity() < received.max_capacity() / 8 {
                                        warn!(
                                            "udp:receive:capacity = {}/{}",
                                            received.capacity(),
                                            received.max_capacity()
                                        );
                                    }

                                    batch.push(ServerCommand::Received(TransportMessage((
                                        addr, message,
                                    ))));
                                }
                                Ok(None) => {}
                                Err(e) => warn!("Codec error: {}", e),
                            }
                        }
                        Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                            match received.send(batch).await {
                                Err(e) => warn!("Publish received error: {}", e),
                                _ => {}
                            }

                            break;
                        }
                        Err(e) => warn!("Receive error: {}", e),
                    }
                }
            }
        });

        // No amount of investigation into graceful exiting has been done.
        tokio::spawn(async move {
            while let Some(TransportMessage((addr, message))) = send_rx.recv().await {
                let mut buffer = Vec::new();
                match message.write(&mut buffer) {
                    Ok(_) => {
                        debug!("{:?} Sending {:?}", addr, buffer.len());

                        match sending.send_to(&buffer, addr).await {
                            Ok(_) => {}
                            Err(e) => warn!("Udp send error: {}", e),
                        }
                    }
                    Err(e) => warn!("Codec error: {}", e),
                }
            }
        });

        Ok(send_tx)
    }
}

pub struct Server<T, R>
where
    T: Transport,
    R: RecordsSink,
{
    transport: T,
    records_sink: Arc<Mutex<R>>,
    sender: Arc<Mutex<Option<Sender<Vec<ServerCommand>>>>>,
}

impl<T: Transport, R: RecordsSink + 'static> Server<T, R> {
    pub fn new(transport: T, records_sink: R) -> Self {
        Self {
            transport,
            records_sink: Arc::new(Mutex::new(records_sink)),
            sender: Arc::new(Mutex::new(None)),
        }
    }

    #[cfg(test)]
    pub async fn received(&self, addr: SocketAddr, message: Message) -> Result<()> {
        self.send(ServerCommand::Received(TransportMessage((addr, message))))
            .await
    }

    pub async fn sync(&self, discovered: Discovered) -> Result<()> {
        self.send(ServerCommand::Begin(discovered)).await
    }

    pub async fn cancel(&self, device_id: DeviceId) -> Result<()> {
        self.send(ServerCommand::Cancel(device_id)).await
    }

    pub async fn run(&self, publish: Sender<ServerEvent>) -> Result<()> {
        let (tx, mut rx) = mpsc::channel::<Vec<ServerCommand>>(1024);

        let sending = self.transport.open(tx.clone()).await?;

        let records_sink = Arc::clone(&self.records_sink);

        let pump = tokio::spawn({
            let tx = tx.clone();
            let mut devices = Devices::new();
            let mut locked = self.sender.lock().await;
            *locked = Some(tx.clone());
            async move {
                while let Some(batch) = rx.recv().await {
                    if tx.capacity() < tx.max_capacity() / 8 {
                        info!(
                            "server-command:capacity = {}/{} ({})",
                            tx.capacity(),
                            tx.max_capacity(),
                            batch.len(),
                        );
                    }
                    for cmd in batch.into_iter() {
                        match handle_server_command::<R>(
                            &cmd,
                            &sending,
                            &publish,
                            &mut devices,
                            &records_sink,
                        )
                        .await
                        {
                            Ok(_) => {}
                            Err(e) => warn!("Server command error: {}", e),
                        }
                    }
                }
            }
        });

        let timer = tokio::spawn(async move {
            let mut interval = time::interval(Duration::from_millis(250));

            loop {
                interval.tick().await;

                match tx.send(vec![ServerCommand::Tick]).await {
                    Err(e) => warn!("Tick error: {}", e),
                    Ok(_) => {}
                }
            }
        });

        tokio::select! {
            _ = pump => Ok(()),
            _ = timer => Ok(())
        }
    }

    async fn send(&self, cmd: ServerCommand) -> Result<()> {
        let locked = self.sender.lock().await;
        Ok(locked
            .as_ref()
            .ok_or_else(|| anyhow!("Sender required"))?
            .send(vec![cmd])
            .await?)
    }
}

fn bind(addr: &SocketAddrV4) -> Result<UdpSocket> {
    use socket2::{Domain, Protocol, Socket, Type};

    let socket = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;

    socket.set_reuse_address(true)?;
    socket.set_nonblocking(true)?;
    socket.bind(&socket2::SockAddr::from(*addr))?;

    Ok(UdpSocket::from_std(socket.into())?)
}

struct Devices {
    devices: HashMap<DeviceId, ConnectedDevice>,
    by_addr: HashMap<SocketAddr, DeviceId>,
}

impl Devices {
    fn new() -> Self {
        Self {
            devices: HashMap::new(),
            by_addr: HashMap::new(),
        }
    }
    fn get_or_add_device(
        &mut self,
        device_id: DeviceId,
        addr: SocketAddr,
    ) -> Option<&mut ConnectedDevice> {
        let entry = self.devices.entry(device_id.clone());
        let entry = entry.or_insert_with(|| ConnectedDevice::new(device_id.clone(), addr));

        self.by_addr.entry(addr).or_insert(device_id.clone());

        Some(entry)
    }

    fn get_device_by_addr(&mut self, addr: &SocketAddr) -> Option<&mut ConnectedDevice> {
        if let Some(device_id) = self.by_addr.get(addr) {
            self.devices.get_mut(device_id)
        } else {
            None
        }
    }

    fn remove_expired(&mut self) {
        for (device_id, addr) in self
            .devices
            .iter()
            .filter(|(_, connected)| connected.is_expired())
            .map(|(device_id, connected)| (device_id.clone(), connected.addr))
            .collect::<Vec<_>>()
        {
            info!("{:?}@{:?} expired", device_id, addr);
            self.by_addr.remove(&addr);
            self.devices.remove(&device_id);
        }
    }

    fn iter(&mut self) -> impl Iterator<Item = &mut ConnectedDevice> {
        self.devices.iter_mut().map(|(_, c)| c)
    }

    fn remove(&mut self, device_id: &DeviceId) -> Option<ConnectedDevice> {
        if let Some((_, connected)) = self.devices.remove_entry(device_id) {
            info!("{:?} removed", device_id);
            self.by_addr.remove(&connected.addr);
            Some(connected)
        } else {
            None
        }
    }
}

async fn handle_server_command<R: RecordsSink>(
    cmd: &ServerCommand,
    sending: &Sender<TransportMessage>,
    publish: &Sender<ServerEvent>,
    devices: &mut Devices,
    records_sink: &Arc<Mutex<R>>,
) -> Result<()> {
    match cmd {
        ServerCommand::Begin(discovered) => {
            let udp_addr = discovered
                .udp_addr
                .ok_or(anyhow::anyhow!("Udp address is required"))?;

            match devices.get_or_add_device(discovered.device_id.clone(), udp_addr) {
                Some(connected) => {
                    let transition = connected.tick()?;
                    connected.apply(transition, publish, sending).await?
                }
                None => {}
            }
        }
        ServerCommand::Received(TransportMessage((addr, message))) => {
            message.log_received();

            match devices.get_device_by_addr(addr) {
                Some(connected) => {
                    let transition = connected.handle(message)?;
                    connected.apply(transition, publish, sending).await?;

                    if let Some(records) = message.numbered_records()? {
                        let sink = records_sink.lock().await;
                        sink.write(&ReceivedRecords {
                            device_id: connected.device_id.clone(),
                            records,
                        })?;
                    }

                    if let Some(progress) = connected.progress() {
                        let publish_progress = match connected.progress_published {
                            Some(last) => Instant::now().sub(last) > Duration::from_secs(1),
                            None => true,
                        };
                        if publish_progress {
                            let started = connected.syncing_started.unwrap_or(SystemTime::now());
                            publish
                                .send(ServerEvent::Progress(
                                    connected.device_id.clone(),
                                    started,
                                    progress,
                                ))
                                .await?;

                            connected.progress_published = Some(Instant::now());
                        }
                    }
                }
                None => todo!(),
            }
        }
        ServerCommand::Tick => {
            devices.remove_expired();

            for connected in devices.iter() {
                let transition = connected.tick()?;
                connected.apply(transition, publish, sending).await?;
            }
        }
        ServerCommand::Cancel(device_id) => {
            if let Some(connected) = devices.remove(device_id) {
                info!("{:?} removed", device_id);
                match connected.state {
                    DeviceState::Synced => {}
                    _ => publish.send(ServerEvent::Failed(device_id.clone())).await?,
                }
            }
        }
    }

    Ok(())
}

async fn transmit(tx: &Sender<TransportMessage>, addr: &SocketAddr, m: Message) -> Result<()> {
    info!("{:?} to {:?} (capacity = {})", m, addr, tx.capacity());

    Ok(tx.send(TransportMessage((addr.clone(), m))).await?)
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use tokio::sync::{mpsc::Receiver, Mutex};

    use super::*;

    #[allow(dead_code)]
    pub struct TokioTransport {
        first: Mutex<Cell<Option<(Sender<TransportMessage>, Receiver<TransportMessage>)>>>,
        second: Mutex<Cell<Option<(Sender<TransportMessage>, Receiver<TransportMessage>)>>>,
    }

    impl TokioTransport {}

    impl Default for TokioTransport {
        fn default() -> Self {
            let (inbox_tx, inbox_rx) = mpsc::channel::<TransportMessage>(256);
            let (outbox_tx, outbox_rx) = mpsc::channel::<TransportMessage>(256);

            Self {
                first: Mutex::new(Cell::new(Some((inbox_tx, outbox_rx)))),
                second: Mutex::new(Cell::new(Some((outbox_tx, inbox_rx)))),
            }
        }
    }

    #[async_trait]
    impl Transport for TokioTransport {
        async fn open(
            &self,
            _received: Sender<Vec<ServerCommand>>,
        ) -> Result<Sender<TransportMessage>> {
            let second = self.second.lock().await;
            Ok(second.take().unwrap().0)
        }
    }

    fn test_device() -> ConnectedDevice {
        ConnectedDevice::new(
            DeviceId("test-device".to_owned()),
            SocketAddrV4::new(IP_ALL.into(), 22144).into(),
        )
    }

    #[test]
    pub fn test_server() {
        let transport = TokioTransport::default();
        let _server = Server::new(transport, DevNullSink::default());
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
