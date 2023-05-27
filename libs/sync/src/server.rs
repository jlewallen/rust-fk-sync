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
    sync::mpsc::{self, Receiver},
    sync::Mutex,
    time::{self, Instant},
};
use tracing::*;

use crate::{
    progress::{Progress, RangeProgress},
    proto::{Message, MessageCodec, RecordRange},
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
enum ServerCommand {
    Begin(Discovered),
    Cancel(DeviceId),
    Received(TransportMessage),
    Tick,
}

#[derive(Clone, Debug)]
enum DeviceState {
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

#[derive(Debug)]
enum HasGaps {
    Continuous(RangeInclusive<u64>),
    HasGaps(RangeInclusive<u64>, Vec<RangeInclusive<u64>>),
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
    syncing_started: Option<SystemTime>,
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

                        self.query_requires(publish).await
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

    async fn handle(
        &mut self,
        publish: &Sender<ServerEvent>,
        message: &Message,
    ) -> Result<Option<Message>> {
        self.received_at = Instant::now();
        self.backoff.reset();
        self.waiting_until = None;

        if let DeviceState::Expecting((_deadline, after)) = &self.state {
            self.transition(*Box::clone(after)) // TODO into_inner on unstable
        }

        match message {
            Message::Statistics { nrecords } => {
                self.total_records = Some(*nrecords);
                self.syncing_started = Some(SystemTime::now());
                self.query_requires(publish).await
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
                trace!("{} {:?}", progress, &has_gaps);

                Ok(None)
            }
            Message::Batch { flags: _flags } => self.query_requires(publish).await,
            _ => Ok(None),
        }
    }

    async fn query_requires(&mut self, publish: &Sender<ServerEvent>) -> Result<Option<Message>> {
        if let Some(range) = self.requires() {
            self.transition(DeviceState::expecting(DeviceState::Receiving(
                range.clone(),
            )));
            Ok(Some(Message::Require(range)))
        } else {
            if self.total_records.is_some() {
                self.transition(DeviceState::Synced);
                let elapsed = SystemTime::now().duration_since(self.syncing_started.unwrap())?;
                info!("Syncing took {:?}", elapsed);
                publish
                    .send(ServerEvent::Completed(self.device_id.clone()))
                    .await?;
            }
            Ok(None)
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
pub struct TransportMessage((SocketAddr, Message));

pub type MessageReceiver = Receiver<TransportMessage>;
pub type MessageSender = Sender<TransportMessage>;

#[async_trait]
pub trait Transport {
    async fn open(&self) -> Result<(MessageSender, MessageReceiver)>;
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
    async fn open(&self) -> Result<(MessageSender, MessageReceiver)> {
        let listening_addr = SocketAddrV4::new(IP_ALL.into(), self.port);
        let receiving = Arc::new(bind(&listening_addr)?);
        let sending = receiving.clone();

        info!("listening on {}", listening_addr);

        // This were chosen arbitrarily.
        let (send_tx, mut send_rx) = mpsc::channel::<TransportMessage>(256);
        let (recv_tx, recv_rx) = mpsc::channel::<TransportMessage>(256);

        // No amount of investigation into graceful exiting has been done.
        tokio::spawn(async move {
            loop {
                let mut codec = MessageCodec::default();
                loop {
                    let mut buffer = vec![0u8; 4096];

                    match receiving.recv_from(&mut buffer[..]).await {
                        Ok((len, addr)) => {
                            trace!("{:?} Received {:?}", addr, len);

                            match codec.try_read(&buffer[..len]) {
                                Ok(Some(message)) => {
                                    match recv_tx.send(TransportMessage((addr, message))).await {
                                        Err(e) => warn!("Publish received error: {}", e),
                                        _ => {}
                                    }

                                    break;
                                }
                                Ok(None) => {}
                                Err(e) => warn!("Codec error: {}", e),
                            }
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
                        trace!("{:?} Sending {:?}", addr, buffer.len());

                        match sending.send_to(&buffer, addr).await {
                            Ok(_) => {}
                            Err(e) => warn!("Udp send error: {}", e),
                        }
                    }
                    Err(e) => warn!("Codec error: {}", e),
                }
            }
        });

        Ok((send_tx, recv_rx))
    }
}

pub struct Server<T>
where
    T: Transport,
{
    transport: T,
    sender: Arc<Mutex<Option<Sender<ServerCommand>>>>,
}

impl<T: Transport> Server<T> {
    pub fn new(transport: T) -> Self {
        Self {
            transport,
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
        let (sending, mut receiving) = self.transport.open().await?;

        let mut device_id_by_addr: HashMap<SocketAddr, DeviceId> = HashMap::new();
        let mut devices: HashMap<DeviceId, ConnectedDevice> = HashMap::new();
        let (tx, mut rx) = mpsc::channel::<ServerCommand>(1024);

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
                        Err(e) => warn!("Server command error: {}", e),
                    }
                }
            }
        });

        let receive = tokio::spawn({
            let tx = tx.clone();

            async move {
                loop {
                    match receive_and_process(&mut receiving, &tx).await {
                        Err(e) => warn!("Receive and process error: {}", e),
                        Ok(_) => {}
                    }
                }
            }
        });

        let timer = tokio::spawn(async move {
            let mut interval = time::interval(Duration::from_millis(250));

            loop {
                interval.tick().await;

                match tx.send(ServerCommand::Tick).await {
                    Err(e) => warn!("Tick error: {}", e),
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

    async fn send(&self, cmd: ServerCommand) -> Result<()> {
        let locked = self.sender.lock().await;
        Ok(locked
            .as_ref()
            .ok_or_else(|| anyhow!("Sender required"))?
            .send(cmd)
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

async fn handle_server_command(
    cmd: &ServerCommand,
    sending: &MessageSender,
    publish: &Sender<ServerEvent>,
    devices: &mut HashMap<DeviceId, ConnectedDevice>,
    device_id_by_addr: &mut HashMap<SocketAddr, DeviceId>,
) -> Result<()> {
    match cmd {
        ServerCommand::Begin(discovered) => {
            let udp_addr = discovered
                .udp_addr
                .ok_or(anyhow::anyhow!("Udp address is required"))?;

            let entry = devices.entry(discovered.device_id.clone());
            let entry = entry
                .or_insert_with(|| ConnectedDevice::new(discovered.device_id.clone(), udp_addr));

            device_id_by_addr
                .entry(udp_addr)
                .or_insert(discovered.device_id.clone());

            if let Some(message) = entry.tick(publish).await? {
                transmit(sending, &udp_addr, message).await?;
                publish
                    .send(ServerEvent::Began(discovered.device_id.clone()))
                    .await?;
            }
        }
        ServerCommand::Received(TransportMessage((addr, message))) => {
            message.log_received();

            if let Some(device_id) = device_id_by_addr.get(addr) {
                match devices.get_mut(device_id) {
                    Some(connected) => {
                        let reply = connected.handle(publish, message).await?;

                        if let Some(reply) = reply {
                            transmit(sending, &addr, reply).await?;
                        }

                        if let Some(progress) = connected.progress() {
                            let started = connected.syncing_started.unwrap_or(SystemTime::now());
                            publish
                                .send(ServerEvent::Progress(device_id.clone(), started, progress))
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
            }

            for (_, connected) in devices.iter_mut() {
                if let Some(message) = connected.tick(publish).await? {
                    transmit(&sending, &connected.addr, message).await?;
                }
            }
        }
        ServerCommand::Cancel(device_id) => {
            if let Some(connected) = devices.remove_entry(device_id) {
                info!("{:?} removed", device_id);
                device_id_by_addr.remove(&connected.1.addr);
                match connected.1.state {
                    DeviceState::Synced => {}
                    _ => publish.send(ServerEvent::Failed(device_id.clone())).await?,
                }
            } else {
                info!("{:?} unknown", device_id);
            }
        }
    }

    Ok(())
}

async fn try_receive_one(receiving: &mut MessageReceiver) -> Result<TransportMessage> {
    Ok(receiving
        .recv()
        .await
        .ok_or_else(|| anyhow!("Expected message"))?)
}

async fn receive_and_process(
    receiving: &mut MessageReceiver,
    tx: &Sender<ServerCommand>,
) -> Result<()> {
    Ok(tx
        .send(ServerCommand::Received(try_receive_one(receiving).await?))
        .await?)
}

async fn transmit(tx: &MessageSender, addr: &SocketAddr, m: Message) -> Result<()> {
    info!("{:?} to {:?}", m, addr);

    Ok(tx.send(TransportMessage((addr.clone(), m))).await?)
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use tokio::sync::Mutex;

    use super::*;

    pub struct TokioTransport {
        first: Mutex<Cell<Option<(MessageSender, MessageReceiver)>>>,
        second: Mutex<Cell<Option<(MessageSender, MessageReceiver)>>>,
    }

    impl TokioTransport {
        fn new() -> Self {
            Self::default()
        }
    }

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
        async fn open(&self) -> Result<(MessageSender, MessageReceiver)> {
            let second = self.second.lock().await;
            Ok(second.take().unwrap())
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
        let server = Server::new(transport);
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
