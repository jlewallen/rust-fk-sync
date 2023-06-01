use anyhow::{anyhow, Result};
use async_trait::async_trait;
use backoff::{backoff::Backoff, ExponentialBackoff, ExponentialBackoffBuilder};
use discovery::{DeviceId, Discovered};
use range_set_blaze::prelude::*;
use std::{
    collections::HashMap,
    net::{SocketAddr, SocketAddrV4},
    ops::RangeInclusive,
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
    #[allow(dead_code)]
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
    fn new_with_batch_size(device_id: DeviceId, addr: SocketAddr, batch_size: u64) -> Self {
        Self {
            device_id,
            addr,
            batch_size,
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

    fn new(device_id: DeviceId, addr: SocketAddr) -> Self {
        Self::new_with_batch_size(device_id, addr, 5000)
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

    async fn apply<S: SendTransport>(
        &mut self,
        transition: Transition,
        publish: &Sender<ServerEvent>,
        sender: &S,
    ) -> Result<()> {
        match transition {
            Transition::None => Ok(()),
            Transition::Direct(state) => {
                self.transition(state, publish).await?;

                Ok(())
            }
            Transition::SendOnly(sending) => {
                sender
                    .send(TransportMessage((self.addr.clone(), sending)))
                    .await?;

                Ok(())
            }
            Transition::Send(sending, state) => {
                sender
                    .send(TransportMessage((self.addr.clone(), sending)))
                    .await?;

                self.transition(state, publish).await?;

                Ok(())
            }
        }
    }
}

#[derive(Debug)]
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
pub trait SendTransport: Send + Sync + 'static {
    async fn send(&self, message: TransportMessage) -> Result<()>;
}

#[async_trait]
pub trait ReceiveTransport: Send + Sync + 'static {
    async fn recv(&self) -> Result<Option<Vec<TransportMessage>>>;
}

#[async_trait]
pub trait Transport {
    type Send: SendTransport;
    type Receive: ReceiveTransport;

    async fn open(&self) -> Result<(Self::Send, Self::Receive)>;
}

#[derive(Clone)]
pub struct OpenUdp {
    socket: Arc<UdpSocket>,
}

#[async_trait]
impl SendTransport for OpenUdp {
    async fn send(&self, message: TransportMessage) -> Result<()> {
        let TransportMessage((addr, message)) = message;
        let mut buffer = Vec::new();
        message.write(&mut buffer)?;

        debug!("{:?} Sending {:?}", addr, buffer.len());

        self.socket.send_to(&buffer, addr).await?;

        Ok(())
    }
}

#[async_trait]
impl ReceiveTransport for OpenUdp {
    async fn recv(&self) -> Result<Option<Vec<TransportMessage>>> {
        let mut codec = MessageCodec::default();
        let mut batch: Vec<TransportMessage> = Vec::new();

        self.socket.readable().await.expect("Oops");

        /*
        if let Some(last_packet) = last_packet {
            let elapsed = Instant::now().sub(last_packet);
            if elapsed > Duration::from_millis(500) {
                info!("last packet {:?}", elapsed);
            }
        }
        last_packet = Some(Instant::now());
        */

        loop {
            let mut buffer = vec![0u8; 4096];

            match self.socket.try_recv_from(&mut buffer[..]) {
                Ok((len, addr)) => {
                    trace!("{:?} Received {:?}", addr, len);

                    match codec.try_read(&buffer[..len]) {
                        Ok(Some(message)) => {
                            if let Message::Batch { flags: _flags } = message {
                                info!("{:?} Batch", addr,)
                            }

                            batch.push(TransportMessage((addr, message)));
                        }
                        Ok(None) => {}
                        Err(e) => warn!("Codec error: {}", e),
                    }
                }
                Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    return Ok(Some(batch));
                }
                Err(e) => warn!("Receive error: {}", e),
            }
        }
    }
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
    type Send = OpenUdp;
    type Receive = OpenUdp;

    async fn open(&self) -> Result<(OpenUdp, OpenUdp)> {
        let listening_addr = SocketAddrV4::new(IP_ALL.into(), self.port);
        let socket = Arc::new(bind(&listening_addr)?);

        info!("listening on {}", listening_addr);

        let sender = OpenUdp { socket };

        let receiver = sender.clone();

        Ok((sender, receiver))
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

        let (send, receive) = self.transport.open().await?;

        let (sink_tx, mut sink_rx) = mpsc::channel::<ReceivedRecords>(1024);
        let records_sink = Arc::clone(&self.records_sink);

        let drain_sink = tokio::spawn({
            async move {
                while let Some(received) = sink_rx.recv().await {
                    let sink = records_sink.lock().await;
                    match sink.write(&received) {
                        Err(e) => warn!("Write records error: {:?}", e),
                        Ok(_) => (),
                    }
                }
            }
        });

        let receiver = tokio::spawn({
            let tx = tx.clone();
            async move {
                while let Ok(Some(received)) = receive.recv().await {
                    match tx
                        .send(
                            received
                                .into_iter()
                                .map(|m| ServerCommand::Received(m))
                                .collect(),
                        )
                        .await
                    {
                        Err(e) => warn!("Error forwarding received: {:?}", e),
                        Ok(_) => {}
                    }
                }
            }
        });

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
                    for cmd in batch.iter() {
                        match handle_server_command::<R, T::Send>(
                            cmd,
                            &send,
                            &publish,
                            &sink_tx,
                            &mut devices,
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
            _ = drain_sink => Ok(()),
            _ = receiver => Ok(()),
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

async fn handle_server_command<R: RecordsSink, S: SendTransport>(
    cmd: &ServerCommand,
    sending: &S,
    publish: &Sender<ServerEvent>,
    sink: &Sender<ReceivedRecords>,
    devices: &mut Devices,
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
                        sink.send(ReceivedRecords {
                            device_id: connected.device_id.clone(),
                            records,
                        })
                        .await?;
                    }

                    if let Some(progress) = connected.progress() {
                        publish
                            .send(ServerEvent::Progress(
                                connected.device_id.clone(),
                                connected.syncing_started.unwrap_or(SystemTime::now()),
                                progress,
                            ))
                            .await?;
                        connected.progress_published = Some(Instant::now());
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

#[cfg(test)]
mod tests {
    use tokio::sync::Mutex;

    use super::*;

    #[allow(dead_code)]
    #[derive(Default)]
    pub struct VectorTransport {
        open: OpenVectorTransport,
    }

    impl VectorTransport {}

    #[derive(Default, Clone)]
    pub struct OpenVectorTransport {
        outbox: Arc<Mutex<Vec<TransportMessage>>>,
        inbox: Arc<Mutex<Vec<TransportMessage>>>,
    }

    #[async_trait]
    impl SendTransport for OpenVectorTransport {
        async fn send(&self, message: TransportMessage) -> Result<()> {
            let mut outbox = self.outbox.lock().await;
            outbox.push(message);

            Ok(())
        }
    }

    #[async_trait]
    impl ReceiveTransport for OpenVectorTransport {
        async fn recv(&self) -> Result<Option<Vec<TransportMessage>>> {
            let mut inbox = self.inbox.lock().await;
            Ok(inbox.pop().map(|m| vec![m]))
        }
    }

    #[async_trait]
    impl Transport for VectorTransport {
        type Send = OpenVectorTransport;
        type Receive = OpenVectorTransport;

        async fn open(&self) -> Result<(Self::Send, Self::Receive)> {
            Ok((self.open.clone(), self.open.clone()))
        }
    }

    fn test_device() -> ConnectedDevice {
        ConnectedDevice::new_with_batch_size(
            DeviceId("test-device".to_owned()),
            SocketAddrV4::new(IP_ALL.into(), 22144).into(),
            10000,
        )
    }

    #[test]
    pub fn test_server() {
        let transport = VectorTransport::default();
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
