//! virtio-vsock: stream sockets between midgard (the host, CID 2) and the guest.
//!
//! midgard itself is the host end; there is no AF_VSOCK on the host. Host tasks
//! connect to ports that the guest listens on and get a [`VsockStream`] (AsyncRead and
//! AsyncWrite); the guest cannot connect to the host. Both directions use virtio-vsock's
//! credit-based flow control.

use std::collections::{HashMap, VecDeque};
use std::future::Future;
use std::io;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, ready};

use event_listener::{Event, EventListener};
use futures_lite::{AsyncRead, AsyncWrite};
use midgard_hv::GuestMemory;
use zerocopy::little_endian::{U16, U32, U64};
use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout};

use super::{Chain, Device, Error, Queue, read_config};
use crate::board::Board;
use crate::logging::log;

const NAME: &str = "virtio-vsock";
const DEVICE_ID: u32 = 19;
const RX_QUEUE: usize = 0;
const TX_QUEUE: usize = 1;
const QUEUE_SIZE: u16 = 256;

const HOST_CID: u64 = midgard_protocol::HOST_CID as u64;
const GUEST_CID: u64 = midgard_protocol::GUEST_CID as u64;

const TYPE_STREAM: u16 = 1;

const OP_REQUEST: u16 = 1;
const OP_RESPONSE: u16 = 2;
const OP_RST: u16 = 3;
const OP_SHUTDOWN: u16 = 4;
const OP_RW: u16 = 5;
const OP_CREDIT_UPDATE: u16 = 6;
const OP_CREDIT_REQUEST: u16 = 7;

/// SHUTDOWN flags: the sender will not receive / send anymore.
const SHUTDOWN_RCV: u32 = 1;
const SHUTDOWN_SEND: u32 = 2;

/// How much data from the guest we buffer per connection (our credit).
const BUFFER_SIZE: u32 = 256 * 1024;
/// How much data to the guest we buffer per connection before writers wait.
const MAX_PENDING_TX: usize = 256 * 1024;
/// The first port for host-side connections.
const FIRST_HOST_PORT: u32 = 0x4000_0000;

/// struct virtio_vsock_hdr.
#[derive(Debug, Clone, Copy, Default, FromBytes, IntoBytes, Immutable, KnownLayout)]
#[repr(C)]
struct Header {
    src_cid: U64,
    dst_cid: U64,
    src_port: U32,
    dst_port: U32,
    len: U32,
    type_: U16,
    op: U16,
    flags: U32,
    buf_alloc: U32,
    fwd_cnt: U32,
}

const HEADER_SIZE: usize = size_of::<Header>();
const _: () = assert!(HEADER_SIZE == 44);

/// (host port, guest port)
type Key = (u32, u32);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ConnectState {
    Connecting,
    Connected,
    /// Refused or reset.
    Closed,
}

/// The state of a connection that host tasks see.
struct StreamState {
    connect: ConnectState,
    /// Data from the guest that the host did not read yet.
    from_guest: VecDeque<u8>,
    /// The guest will not send anymore (EOF after `from_guest`).
    guest_done: bool,
    /// Data for the guest that the device did not send yet.
    to_guest: VecDeque<u8>,
    /// The host will not send anymore (after `to_guest`).
    host_done: bool,
    /// Bytes that the host read (the guest's view of our consumption).
    consumed: u32,
}

struct StreamShared {
    state: Mutex<StreamState>,
    changed: Event,
}

/// The device's view of a connection.
struct Connection {
    shared: Arc<StreamShared>,
    /// Bytes that we sent to the guest.
    tx_count: u32,
    /// The guest's credit: its buffer size and how much of our data it consumed.
    peer_buf_alloc: u32,
    peer_fwd_cnt: u32,
    /// The consumption that we last reported to the guest.
    reported_fwd_cnt: u32,
    /// We sent SHUTDOWN(SEND).
    shutdown_sent: bool,
}

impl Connection {
    fn peer_credit(&self) -> u32 {
        self.peer_buf_alloc
            .saturating_sub(self.tx_count.wrapping_sub(self.peer_fwd_cnt))
    }
}

pub(crate) struct Vsock {
    connections: HashMap<Key, Connection>,
    /// Control packets for the guest (without data).
    control: VecDeque<Header>,
    next_port: u32,
}

impl Default for Vsock {
    fn default() -> Self {
        Self {
            connections: HashMap::new(),
            control: VecDeque::new(),
            next_port: FIRST_HOST_PORT,
        }
    }
}

fn header(key: Key, op: u16) -> Header {
    Header {
        src_cid: HOST_CID.into(),
        dst_cid: GUEST_CID.into(),
        src_port: key.0.into(),
        dst_port: key.1.into(),
        type_: TYPE_STREAM.into(),
        op: op.into(),
        buf_alloc: BUFFER_SIZE.into(),
        ..Default::default()
    }
}

impl Vsock {
    fn queue_control(&mut self, key: Key, op: u16, flags: u32) {
        let fwd_cnt = self
            .connections
            .get_mut(&key)
            .map(|c| {
                let consumed = c.shared.state.lock().unwrap().consumed;
                c.reported_fwd_cnt = consumed;
                consumed
            })
            .unwrap_or(0);
        let mut h = header(key, op);
        h.flags.set(flags);
        h.fwd_cnt.set(fwd_cnt);
        self.control.push_back(h);
    }

    fn reset(&mut self, key: Key) {
        if let Some(connection) = self.connections.remove(&key) {
            let mut state = connection.shared.state.lock().unwrap();
            state.connect = ConnectState::Closed;
            state.guest_done = true;
            connection.shared.changed.notify(usize::MAX);
        }
    }

    /// Handles a packet from the guest.
    fn receive(&mut self, h: Header, data: Vec<u8>) {
        let key = (h.dst_port.get(), h.src_port.get());
        if h.dst_cid.get() != HOST_CID || h.type_.get() != TYPE_STREAM {
            if h.op.get() != OP_RST {
                self.control.push_back(header(key, OP_RST));
            }
            return;
        }
        let Some(connection) = self.connections.get_mut(&key) else {
            // An unknown connection (or a request: nothing listens on the host).
            if h.op.get() != OP_RST {
                self.control.push_back(header(key, OP_RST));
            }
            return;
        };
        connection.peer_buf_alloc = h.buf_alloc.get();
        connection.peer_fwd_cnt = h.fwd_cnt.get();
        let shared = connection.shared.clone();
        match h.op.get() {
            OP_RESPONSE => {
                let mut state = shared.state.lock().unwrap();
                if state.connect == ConnectState::Connecting {
                    state.connect = ConnectState::Connected;
                }
                shared.changed.notify(usize::MAX);
            }
            OP_RST => self.reset(key),
            OP_SHUTDOWN => {
                let mut state = shared.state.lock().unwrap();
                if h.flags.get() & SHUTDOWN_SEND != 0 {
                    state.guest_done = true;
                }
                if h.flags.get() & SHUTDOWN_RCV != 0 {
                    // The guest does not want our data anymore.
                    state.to_guest.clear();
                    state.host_done = true;
                }
                let both = state.guest_done && state.host_done;
                shared.changed.notify(usize::MAX);
                drop(state);
                if both {
                    // Close the connection for good.
                    self.control.push_back(header(key, OP_RST));
                    self.reset(key);
                }
            }
            OP_RW => {
                let mut state = shared.state.lock().unwrap();
                state.from_guest.extend(&data);
                shared.changed.notify(usize::MAX);
            }
            // The credit was updated above; data may flow again.
            OP_CREDIT_UPDATE => {
                shared.changed.notify(usize::MAX);
            }
            OP_CREDIT_REQUEST => self.queue_control(key, OP_CREDIT_UPDATE, 0),
            OP_REQUEST => self.control.push_back(header(key, OP_RST)),
            _ => {}
        }
    }

    /// Picks the next packet for the guest that fits into `room` bytes of payload.
    fn next_packet(&mut self, room: usize) -> Option<(Header, Vec<u8>)> {
        if let Some(h) = self.control.pop_front() {
            return Some((h, Vec::new()));
        }
        let mut finished = Vec::new();
        let mut packet = None;
        for (&key, connection) in &mut self.connections {
            let shared = connection.shared.clone();
            let mut state = shared.state.lock().unwrap();
            if state.connect != ConnectState::Connected {
                continue;
            }
            // Report our consumption once the guest's view of our buffer is half full.
            let unreported = state.consumed.wrapping_sub(connection.reported_fwd_cnt);
            let credit = connection.peer_credit() as usize;
            if !state.to_guest.is_empty() && credit > 0 {
                let count = state.to_guest.len().min(credit).min(room);
                let data: Vec<u8> = state.to_guest.drain(..count).collect();
                connection.tx_count = connection.tx_count.wrapping_add(count as u32);
                connection.reported_fwd_cnt = state.consumed;
                let mut h = header(key, OP_RW);
                h.len.set(count as u32);
                h.fwd_cnt.set(state.consumed);
                shared.changed.notify(usize::MAX);
                packet = Some((h, data));
                break;
            }
            if unreported >= BUFFER_SIZE / 2 {
                connection.reported_fwd_cnt = state.consumed;
                let mut h = header(key, OP_CREDIT_UPDATE);
                h.fwd_cnt.set(state.consumed);
                packet = Some((h, Vec::new()));
                break;
            }
            if state.host_done && state.to_guest.is_empty() && !connection.shutdown_sent {
                connection.shutdown_sent = true;
                let mut h = header(key, OP_SHUTDOWN);
                h.flags
                    .set(SHUTDOWN_SEND | if state.guest_done { SHUTDOWN_RCV } else { 0 });
                h.fwd_cnt.set(state.consumed);
                if state.guest_done {
                    finished.push(key);
                }
                packet = Some((h, Vec::new()));
                break;
            }
        }
        for key in finished {
            self.control.push_back(header(key, OP_RST));
            self.reset(key);
        }
        packet
    }

    /// Writes packets into the guest's receive buffers; returns whether it used any.
    fn fill(&mut self, queues: &mut [Queue], memory: &GuestMemory) -> bool {
        let mut added = false;
        loop {
            // Only take a buffer if there is something to send.
            if self.control.is_empty() && !self.has_pending_data() {
                break;
            }
            let Some(chain) = queues[RX_QUEUE].next_chain(memory, NAME) else {
                break;
            };
            let room = chain.writable_len().saturating_sub(HEADER_SIZE);
            let Some((h, data)) = self.next_packet(room) else {
                // Nothing fits after all (e.g., no credit); return the buffer unused.
                queues[RX_QUEUE].push(memory, chain.head, 0);
                added = true;
                break;
            };
            let mut packet = h.as_bytes().to_vec();
            packet.extend_from_slice(&data);
            let written = chain.write(memory, &packet);
            queues[RX_QUEUE].push(memory, chain.head, written);
            added = true;
        }
        added
    }

    fn has_pending_data(&self) -> bool {
        self.connections.values().any(|c| {
            let state = c.shared.state.lock().unwrap();
            state.connect == ConnectState::Connected
                && ((!state.to_guest.is_empty() && c.peer_credit() > 0)
                    || state.consumed.wrapping_sub(c.reported_fwd_cnt) >= BUFFER_SIZE / 2
                    || (state.host_done && state.to_guest.is_empty() && !c.shutdown_sent))
        })
    }

    fn add_connection(&mut self, key: Key, connect: ConnectState) -> Arc<StreamShared> {
        let shared = Arc::new(StreamShared {
            state: Mutex::new(StreamState {
                connect,
                from_guest: VecDeque::new(),
                guest_done: false,
                to_guest: VecDeque::new(),
                host_done: false,
                consumed: 0,
            }),
            changed: Event::new(),
        });
        self.connections.insert(
            key,
            Connection {
                shared: shared.clone(),
                tx_count: 0,
                peer_buf_alloc: 0,
                peer_fwd_cnt: 0,
                reported_fwd_cnt: 0,
                shutdown_sent: false,
            },
        );
        shared
    }

    /// Starts a connection to `port` of the guest.
    fn connect(&mut self, port: u32) -> (Key, Arc<StreamShared>) {
        let mut key = (self.next_port, port);
        while self.connections.contains_key(&key) {
            self.next_port = self.next_port.wrapping_add(1).max(FIRST_HOST_PORT);
            key.0 = self.next_port;
        }
        self.next_port = self.next_port.wrapping_add(1).max(FIRST_HOST_PORT);
        let shared = self.add_connection(key, ConnectState::Connecting);
        self.control.push_back(header(key, OP_REQUEST));
        (key, shared)
    }
}

/// Reads a packet (header and data) from the readable buffers of a chain. The guest
/// does not send more data than our credit allows.
fn read_packet(chain: &Chain, memory: &GuestMemory) -> Result<(Header, Vec<u8>), Error> {
    let mut bytes = chain.read(memory, HEADER_SIZE + BUFFER_SIZE as usize)?;
    if bytes.len() < HEADER_SIZE {
        return Err(Error::Malformed("packet without a header"));
    }
    let (h, _) = Header::read_from_prefix(&bytes).unwrap();
    let mut data = bytes.split_off(HEADER_SIZE);
    data.truncate(h.len.get() as usize);
    Ok((h, data))
}

impl Device for Vsock {
    fn device_id(&self) -> u32 {
        DEVICE_ID
    }

    fn features(&self) -> u64 {
        0
    }

    fn queue_sizes(&self) -> Vec<u16> {
        // Receive, transmit, events.
        vec![QUEUE_SIZE, QUEUE_SIZE, QUEUE_SIZE]
    }

    fn read_config(&self, offset: u64, data: &mut [u8]) {
        // struct virtio_vsock_config: the guest's CID.
        read_config(&GUEST_CID.to_le_bytes(), offset, data);
    }

    fn process(&mut self, queue: usize, queues: &mut [Queue], memory: &GuestMemory) -> bool {
        let mut added = false;
        if queue == TX_QUEUE {
            added = queues[TX_QUEUE].process(memory, NAME, |chain| {
                match read_packet(chain, memory) {
                    Ok((h, data)) => self.receive(h, data),
                    Err(error) => log!("{NAME}: {error}"),
                }
                0
            });
        }
        // New receive buffers or replies to send.
        self.fill(queues, memory) || added
    }

    fn reset(&mut self) {
        let keys: Vec<Key> = self.connections.keys().copied().collect();
        for key in keys {
            self.reset(key);
        }
        self.control.clear();
    }
}

/// Waits until `ready` returns Some for the state behind `mutex`; `event` is notified
/// when the state changes.
async fn wait_until<T, R>(
    mutex: &Mutex<T>,
    event: &Event,
    mut ready: impl FnMut(&mut T) -> Option<R>,
) -> R {
    loop {
        if let Some(result) = ready(&mut mutex.lock().unwrap()) {
            return result;
        }
        // Check again after listening, such that we do not miss a notification.
        let listener = event.listen();
        if let Some(result) = ready(&mut mutex.lock().unwrap()) {
            return result;
        }
        listener.await;
    }
}

/// A stream connection to a port of the guest, for host tasks.
pub struct VsockStream {
    handle: Arc<StreamHandle>,
    /// What poll_read / poll_write wait for (a change of the connection's state).
    listener: Option<EventListener>,
}

/// Shared by the clones of a [`VsockStream`]; the last one closes the connection.
struct StreamHandle {
    board: Arc<Board>,
    key: Key,
    shared: Arc<StreamShared>,
}

impl VsockStream {
    fn new(board: &Arc<Board>, key: Key, shared: Arc<StreamShared>) -> Self {
        Self {
            handle: Arc::new(StreamHandle {
                board: board.clone(),
                key,
                shared,
            }),
            listener: None,
        }
    }

    /// Connects to `port` of the guest; fails with ConnectionRefused if nothing listens.
    pub(crate) async fn connect(board: &Arc<Board>, port: u32) -> io::Result<Self> {
        let (key, shared) = board
            .with_vsock(|vsock, queues, memory| {
                let connection = vsock.connect(port);
                let added = vsock.fill(queues, memory);
                (connection, added)
            })
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "the VM has no vsock device"))?;
        let stream = Self::new(board, key, shared);
        let shared = &stream.handle.shared;
        let connected = wait_until(&shared.state, &shared.changed, |state| {
            (state.connect != ConnectState::Connecting)
                .then_some(state.connect == ConnectState::Connected)
        })
        .await;
        if !connected {
            return Err(io::ErrorKind::ConnectionRefused.into());
        }
        Ok(stream)
    }

    /// Another handle to the same connection (e.g., for the other direction of a proxy).
    pub fn try_clone(&self) -> Self {
        Self {
            handle: self.handle.clone(),
            listener: None,
        }
    }

    /// No more data from us (the guest sees EOF once it read everything).
    pub fn shutdown_write(&self) {
        self.handle.shared.state.lock().unwrap().host_done = true;
        self.handle.flush_device();
    }

    /// Polls `ready` until it returns Some; waits for changes of the state in between.
    fn poll_state<R>(
        &mut self,
        cx: &mut Context<'_>,
        mut ready: impl FnMut(&mut StreamState) -> Option<R>,
    ) -> Poll<R> {
        loop {
            if let Some(result) = ready(&mut self.handle.shared.state.lock().unwrap()) {
                self.listener = None;
                return Poll::Ready(result);
            }
            match &mut self.listener {
                // Check again after listening, such that we do not miss a notification.
                None => self.listener = Some(self.handle.shared.changed.listen()),
                Some(listener) => {
                    ready!(Pin::new(listener).poll(cx));
                    self.listener = None;
                }
            }
        }
    }
}

impl StreamHandle {
    /// Sends whatever the device can send now.
    fn flush_device(&self) {
        self.board
            .with_vsock(|vsock, queues, memory| ((), vsock.fill(queues, memory)));
    }

    fn close(&self) {
        let key = self.key;
        self.board.with_vsock(|vsock, queues, memory| {
            if vsock.connections.contains_key(&key) {
                vsock.control.push_back(header(key, OP_RST));
                vsock.reset(key);
            }
            ((), vsock.fill(queues, memory))
        });
    }
}

impl AsyncRead for VsockStream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buffer: &mut [u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        if buffer.is_empty() {
            return Poll::Ready(Ok(0));
        }
        let count = ready!(this.poll_state(cx, |state| {
            if state.from_guest.is_empty() && !state.guest_done {
                return None;
            }
            let count = buffer.len().min(state.from_guest.len());
            for (target, byte) in buffer.iter_mut().zip(state.from_guest.drain(..count)) {
                *target = byte;
            }
            state.consumed = state.consumed.wrapping_add(count as u32);
            Some(count)
        }));
        if count > 0 {
            // Tell the guest about the space once enough of it is free.
            this.handle.flush_device();
        }
        Poll::Ready(Ok(count))
    }
}

impl AsyncWrite for VsockStream {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        data: &[u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        let count = ready!(this.poll_state(cx, |state| {
            if state.connect == ConnectState::Closed || state.host_done {
                return Some(Err(io::Error::from(io::ErrorKind::BrokenPipe)));
            }
            if state.to_guest.len() >= MAX_PENDING_TX {
                return None;
            }
            let count = data.len().min(MAX_PENDING_TX - state.to_guest.len());
            state.to_guest.extend(&data[..count]);
            Some(Ok(count))
        }))?;
        this.handle.flush_device();
        Poll::Ready(Ok(count))
    }

    fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_close(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.shutdown_write();
        Poll::Ready(Ok(()))
    }
}

impl Drop for StreamHandle {
    fn drop(&mut self) {
        self.close();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A packet from the guest on `key`.
    fn from_guest(key: Key, op: u16) -> Header {
        Header {
            src_cid: GUEST_CID.into(),
            dst_cid: HOST_CID.into(),
            src_port: key.1.into(),
            dst_port: key.0.into(),
            type_: TYPE_STREAM.into(),
            op: op.into(),
            buf_alloc: 4096.into(),
            ..Default::default()
        }
    }

    #[test]
    fn connect_and_exchange_data() {
        let mut vsock = Vsock::default();
        let (key, shared) = vsock.connect(1024);
        assert_eq!(key.1, 1024);
        assert_eq!(vsock.next_packet(0).unwrap().0.op.get(), OP_REQUEST);
        assert_eq!(
            shared.state.lock().unwrap().connect,
            ConnectState::Connecting
        );

        vsock.receive(from_guest(key, OP_RESPONSE), Vec::new());
        assert_eq!(
            shared.state.lock().unwrap().connect,
            ConnectState::Connected
        );

        vsock.receive(from_guest(key, OP_RW), b"hello".to_vec());
        assert_eq!(shared.state.lock().unwrap().from_guest, b"hello");

        // Data for the guest goes out within the guest's credit and the buffer's room.
        shared.state.lock().unwrap().to_guest.extend(b"world");
        let (h, data) = vsock.next_packet(3).unwrap();
        assert_eq!(h.op.get(), OP_RW);
        assert_eq!(data, b"wor");
        assert_eq!(vsock.next_packet(64).unwrap().1, b"ld");
        assert!(vsock.next_packet(64).is_none());

        vsock.receive(from_guest(key, OP_RST), Vec::new());
        let state = shared.state.lock().unwrap();
        assert_eq!(state.connect, ConnectState::Closed);
        assert!(state.guest_done);
    }

    #[test]
    fn refuses_connections_from_the_guest() {
        let mut vsock = Vsock::default();
        vsock.receive(from_guest((1025, 5000), OP_REQUEST), Vec::new());
        let (h, _) = vsock.next_packet(0).unwrap();
        assert_eq!(h.op.get(), OP_RST);
        assert_eq!((h.src_port.get(), h.dst_port.get()), (1025, 5000));
    }
}
