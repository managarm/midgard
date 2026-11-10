//! User-mode networking (like QEMU's slirp): the guest's packets end in midgard, and
//! midgard makes the connections with ordinary sockets of the host.
//!
//! The guest is 10.0.2.15/24. 10.0.2.2 is the gateway and the host itself (its
//! loopback); 10.0.2.3 forwards DNS to the host's resolver. smoltcp answers ARP,
//! reassembles and fragments IP packets, and terminates the guest's TCP connections and
//! UDP flows under their original destination addresses ("any IP"); we make the same
//! connections and send the same datagrams from the host and copy the data. Pings go
//! through the host's ICMP sockets (see [`super::ping`]); midgard answers those to
//! 10.0.2.2 and 10.0.2.3 itself. If the host cannot reach a destination, the guest gets
//! the ICMP error that a router (or the destination) would send.

use std::collections::{HashMap, HashSet, VecDeque};
use std::future::Future;
use std::io::{self, Read, Write};
use std::net::{Ipv4Addr, Shutdown, SocketAddr, SocketAddrV4, TcpStream, UdpSocket};
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use async_channel::{Receiver, Sender};
use async_executor::Executor;
use async_io::{Async, Timer};
use futures_lite::{StreamExt, future};

use smoltcp::iface::{Config, Interface, SocketHandle, SocketSet};
use smoltcp::phy::{self, ChecksumCapabilities, DeviceCapabilities, Medium};
use smoltcp::socket::{raw, tcp, udp};
use smoltcp::time::Instant;
use smoltcp::wire::{
    EthernetAddress, EthernetFrame, EthernetProtocol, HardwareAddress, Icmpv4DstUnreachable,
    Icmpv4Packet, Icmpv4Repr, IpAddress, IpCidr, IpEndpoint, IpProtocol, IpVersion, Ipv4Packet,
    Ipv4Repr, TcpPacket,
};

use crate::host::ping::PingSocket;
use crate::logging::{ReportOnce, log};

pub(crate) const GUEST_IP: Ipv4Addr = Ipv4Addr::new(10, 0, 2, 15);
pub(crate) const HOST_IP: Ipv4Addr = Ipv4Addr::new(10, 0, 2, 2);
pub(crate) const DNS_IP: Ipv4Addr = Ipv4Addr::new(10, 0, 2, 3);
const BROADCAST_IP: Ipv4Addr = Ipv4Addr::new(10, 0, 2, 255);
pub(crate) const GUEST_MAC: [u8; 6] = [0x52, 0x54, 0x00, 0x12, 0x34, 0x56];
const GATEWAY_MAC: [u8; 6] = [0x52, 0x55, 0x0A, 0x00, 0x02, 0x02];

/// The kernel parameter that configures the guest's interface.
pub(crate) fn kernel_parameter() -> String {
    format!("ip={GUEST_IP}::{HOST_IP}:255.255.255.0:midgard:eth0:off:{DNS_IP}")
}

const TCP_BUFFER: usize = 64 * 1024;
/// Data from the guest that we buffer per connection while the host does not take it.
const MAX_TO_HOST: usize = 256 * 1024;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// The guest's side of a TCP connection ends if the guest does not answer for this long
/// while we wait for it (e.g., for the ACK of our SYN-ACK).
const TCP_TIMEOUT: Duration = Duration::from_secs(60);
/// The buffers of the smoltcp socket for each destination port of UDP (in each direction;
/// a datagram can be almost 64 KiB large).
const UDP_BUFFER: usize = 256 * 1024;
const UDP_PACKETS: usize = 64;
/// The buffers of smoltcp's socket for ICMP (in each direction).
const ICMP_BUFFER: usize = 256 * 1024;
const ICMP_PACKETS: usize = 64;
/// The bytes of the payload that ICMP errors quote (RFC 792).
const QUOTE_LEN: usize = 8;
/// UDP flows and pings end after they were idle for this long.
const FLOW_IDLE: Duration = Duration::from_secs(60);
/// DNS flows end sooner (like slirp's SO_EXPIREFAST): there is one per query, as resolvers
/// pick a new port for each.
const DNS_IDLE: Duration = Duration::from_secs(10);
const DNS_PORT: u16 = 53;
/// The largest UDP datagram (we read from the host into a buffer of this size).
const MAX_DATAGRAM: usize = 65535;
/// The frames from the guest that smoltcp gets before we empty its sockets (such that
/// their buffers do not overflow).
const BATCH: usize = 32;
/// The longest time that the stack sleeps (it expires idle flows when it wakes up).
const MAX_WAIT: Duration = Duration::from_secs(1);

/// The part of the stack that the virtio-net device talks to.
pub(crate) struct UserNetHandle {
    from_guest: Sender<Vec<u8>>,
}

impl UserNetHandle {
    /// A frame from the guest.
    pub(crate) fn send_from_guest(&self, frame: Vec<u8>) {
        // The channel is unbounded; it only fails once the stack is gone.
        let _ = self.from_guest.try_send(frame);
    }
}

/// Creates the handle for the virtio-net device; [`spawn`] starts the stack once the VM
/// exists.
pub(crate) fn channel() -> (Arc<UserNetHandle>, Receiver<Vec<u8>>) {
    let (sender, receiver) = async_channel::unbounded();
    (Arc::new(UserNetHandle { from_guest: sender }), receiver)
}

/// Hands frames to the guest (to its virtio-net device).
type ToGuest = Box<dyn FnMut(Vec<Vec<u8>>) + Send>;

/// Starts the stack's task; it hands the frames for the guest to `deliver`.
pub(crate) fn spawn(
    executor: &Executor<'static>,
    from_guest: Receiver<Vec<u8>>,
    deliver: impl FnMut(Vec<Vec<u8>>) + Send + 'static,
) {
    executor
        .spawn(UserNet::new(from_guest, Box::new(deliver)).run())
        .detach();
}

/// Connects to `target` on the host (or fails after [`CONNECT_TIMEOUT`]).
async fn connect(target: SocketAddr) -> io::Result<Async<TcpStream>> {
    future::or(Async::<TcpStream>::connect(target), async {
        Timer::after(CONNECT_TIMEOUT).await;
        Err(io::ErrorKind::TimedOut.into())
    })
    .await
}

fn ipv4(address: IpAddress) -> Ipv4Addr {
    match address {
        IpAddress::Ipv4(address) => address,
    }
}

/// The frames that smoltcp receives and sends.
#[derive(Default)]
struct Frames {
    rx: VecDeque<Vec<u8>>,
    tx: Vec<Vec<u8>>,
}

struct RxToken(Vec<u8>);
struct TxToken<'a>(&'a mut Vec<Vec<u8>>);

impl phy::RxToken for RxToken {
    fn consume<R, F: FnOnce(&[u8]) -> R>(self, f: F) -> R {
        f(&self.0)
    }
}

impl phy::TxToken for TxToken<'_> {
    fn consume<R, F: FnOnce(&mut [u8]) -> R>(self, len: usize, f: F) -> R {
        let mut frame = vec![0u8; len];
        let result = f(&mut frame);
        self.0.push(frame);
        result
    }
}

impl phy::Device for Frames {
    type RxToken<'a> = RxToken;
    type TxToken<'a> = TxToken<'a>;

    fn receive(&mut self, _: Instant) -> Option<(RxToken, TxToken<'_>)> {
        let frame = self.rx.pop_front()?;
        Some((RxToken(frame), TxToken(&mut self.tx)))
    }

    fn transmit(&mut self, _: Instant) -> Option<TxToken<'_>> {
        Some(TxToken(&mut self.tx))
    }

    fn capabilities(&self) -> DeviceCapabilities {
        let mut capabilities = DeviceCapabilities::default();
        capabilities.medium = Medium::Ethernet;
        capabilities.max_transmission_unit = 1514;
        capabilities
    }
}

/// What [`peek`] found in a frame from the guest.
enum FlowStart {
    /// A SYN: the guest opens a TCP connection.
    Syn(FlowKey),
    /// A UDP datagram (or its first fragment).
    Udp(FlowKey),
}

/// The guest's end and the destination of a TCP connection or a UDP flow.
type FlowKey = (SocketAddrV4, SocketAddrV4);

/// Looks for the start of TCP connections and for UDP datagrams in a frame from the guest.
fn peek(frame: &[u8]) -> Option<FlowStart> {
    let ethernet = EthernetFrame::new_checked(frame).ok()?;
    if ethernet.ethertype() != EthernetProtocol::Ipv4 {
        return None;
    }
    let ip = Ipv4Packet::new_checked(ethernet.payload()).ok()?;
    // Later fragments do not have the transport protocol's header.
    if ip.frag_offset() != 0 {
        return None;
    }
    let (source, destination) = (ip.src_addr(), ip.dst_addr());
    let payload = ip.payload();
    match ip.next_header() {
        IpProtocol::Tcp if !ip.more_frags() => {
            let tcp = TcpPacket::new_checked(payload).ok()?;
            (tcp.syn() && !tcp.ack()).then(|| {
                FlowStart::Syn((
                    SocketAddrV4::new(source, tcp.src_port()),
                    SocketAddrV4::new(destination, tcp.dst_port()),
                ))
            })
        }
        // The ports are at the start of the header; the first fragment of a datagram has
        // them (but it is not a complete datagram).
        IpProtocol::Udp if payload.len() >= 4 => {
            let port = |offset: usize| u16::from_be_bytes([payload[offset], payload[offset + 1]]);
            Some(FlowStart::Udp((
                SocketAddrV4::new(source, port(0)),
                SocketAddrV4::new(destination, port(2)),
            )))
        }
        _ => None,
    }
}

/// A TCP connection of the guest whose connection on the host is not made yet. The
/// guest's SYN waits for it, such that the guest sees the outcome (e.g., a refused
/// connection) instead of a connection that breaks right after it was established.
struct PendingConnect {
    connect: Pin<Box<dyn Future<Output = io::Result<Async<TcpStream>>> + Send>>,
    /// The frame with the guest's (latest) SYN.
    syn: Vec<u8>,
}

/// A TCP connection that smoltcp terminates and its connection on the host.
struct TcpFlow {
    key: FlowKey,
    stream: Async<TcpStream>,
    /// The connection on the host failed; we reset the guest's.
    host_failed: bool,
    to_host: Vec<u8>,
    host_eof: bool,
    shut_down_write: bool,
}

/// A UDP flow of the guest (from one of its ports to a destination) and its socket on
/// the host.
struct UdpFlow {
    /// Unconnected (connected UDP sockets are not reliable on all hosts, e.g., managarm);
    /// we filter replies by their source.
    socket: Async<UdpSocket>,
    /// Where the datagrams go on the host.
    target: SocketAddrV4,
    last_used: std::time::Instant,
}

/// Problems that are reported once (see [`ReportOnce`]): the guest can cause them over
/// and over.
#[derive(PartialEq, Eq, PartialOrd, Ord)]
enum Problem {
    UdpSocket,
    SendDatagram,
    ReceiveDatagram,
    PingSocket,
    ReceivePing,
}

/// Logs a [`Problem`] the first time that it occurs.
macro_rules! report {
    ($reported:expr, $problem:expr, $($arg:tt)*) => {
        if $reported.first($problem) {
            log!("{} (further errors are not reported)", format_args!($($arg)*));
        }
    };
}

/// The pings of the guest with one identifier to one destination, and the host socket
/// that forwards them.
struct PingFlow {
    socket: PingSocket,
    /// The guest's address (where the replies go).
    guest: Ipv4Addr,
    last_used: std::time::Instant,
}

/// An echo request of the guest.
struct EchoRequest {
    guest: Ipv4Addr,
    destination: Ipv4Addr,
    ident: u16,
    seq_no: u16,
    data: Vec<u8>,
}

/// The echo request in an IP packet (from smoltcp's raw socket).
fn parse_echo_request(packet: &[u8]) -> Option<EchoRequest> {
    let ip = Ipv4Packet::new_checked(packet).ok()?;
    let icmp = Icmpv4Packet::new_checked(ip.payload()).ok()?;
    match Icmpv4Repr::parse(&icmp, &ChecksumCapabilities::default()).ok()? {
        Icmpv4Repr::EchoRequest {
            ident,
            seq_no,
            data,
        } => Some(EchoRequest {
            guest: ip.src_addr(),
            destination: ip.dst_addr(),
            ident,
            seq_no,
            data: data.to_vec(),
        }),
        _ => None,
    }
}

/// The IP header of a packet of the guest in `frame` and the start of its payload (what
/// an ICMP error quotes).
fn quote(frame: &[u8]) -> Option<(Ipv4Repr, Vec<u8>)> {
    let ethernet = EthernetFrame::new_checked(frame).ok()?;
    let ip = Ipv4Packet::new_checked(ethernet.payload()).ok()?;
    let header = Ipv4Repr::parse(&ip, &ChecksumCapabilities::ignored()).ok()?;
    let payload = ip.payload();
    Some((header, payload[..payload.len().min(QUOTE_LEN)].to_vec()))
}

/// An IP packet with an ICMP message from `source` to `destination`.
fn icmp_packet(source: Ipv4Addr, destination: Ipv4Addr, icmp: &Icmpv4Repr) -> Vec<u8> {
    let checksums = ChecksumCapabilities::default();
    let ip = Ipv4Repr {
        src_addr: source,
        dst_addr: destination,
        next_header: IpProtocol::Icmp,
        payload_len: icmp.buffer_len(),
        hop_limit: 64,
    };
    let mut packet = vec![0u8; ip.buffer_len() + icmp.buffer_len()];
    let mut ip_packet = Ipv4Packet::new_unchecked(&mut packet[..]);
    ip.emit(&mut ip_packet, &checksums);
    icmp.emit(
        &mut Icmpv4Packet::new_unchecked(ip_packet.payload_mut()),
        &checksums,
    );
    packet
}

struct UserNet {
    from_guest: Pin<Box<Receiver<Vec<u8>>>>,
    /// Frames that we received from the guest but did not look at yet.
    incoming: Vec<Vec<u8>>,
    /// Wakes the stack up for smoltcp's timers.
    timer: Timer,
    deliver: ToGuest,
    frames: Frames,
    iface: Interface,
    sockets: SocketSet<'static>,
    connecting: HashMap<FlowKey, PendingConnect>,
    tcp: HashMap<SocketHandle, TcpFlow>,
    tcp_keys: HashMap<FlowKey, SocketHandle>,
    /// smoltcp's UDP socket for each destination port (it receives the datagrams to all
    /// addresses).
    udp_ports: HashMap<u16, SocketHandle>,
    udp: HashMap<FlowKey, UdpFlow>,
    /// smoltcp's socket for ICMP: it receives the guest's ICMP messages and sends ours
    /// (under any source address).
    icmp_socket: SocketHandle,
    /// By identifier and destination.
    pings: HashMap<(u16, Ipv4Addr), PingFlow>,
    dns_server: Ipv4Addr,
    scratch: Vec<u8>,
    reported: ReportOnce<Problem>,
}

/// The host's first IPv4 nameserver.
fn host_dns_server() -> Ipv4Addr {
    std::fs::read_to_string("/etc/resolv.conf")
        .ok()
        .and_then(|resolv| {
            resolv.lines().find_map(|line| {
                let mut words = line.split_whitespace();
                (words.next() == Some("nameserver"))
                    .then(|| words.next()?.parse().ok())
                    .flatten()
            })
        })
        .unwrap_or(Ipv4Addr::new(8, 8, 8, 8))
}

impl UserNet {
    fn new(from_guest: Receiver<Vec<u8>>, deliver: ToGuest) -> Self {
        let mut frames = Frames::default();
        let mut config = Config::new(HardwareAddress::Ethernet(EthernetAddress(GATEWAY_MAC)));
        config.random_seed = crate::clock::host_ns();
        let mut iface = Interface::new(config, &mut frames, Instant::now());
        iface.update_ip_addrs(|addresses| {
            for ip in [HOST_IP, DNS_IP] {
                let _ = addresses.push(IpCidr::new(IpAddress::Ipv4(ip), 24));
            }
        });
        // Accept packets for all addresses (the guest's connections to the world).
        iface.set_any_ip(true);
        let _ = iface.routes_mut().add_default_ipv4_route(HOST_IP);
        let mut sockets = SocketSet::new(Vec::new());
        let icmp_socket = sockets.add(raw::Socket::new(
            Some(IpVersion::Ipv4),
            Some(IpProtocol::Icmp),
            raw::PacketBuffer::new(
                vec![raw::PacketMetadata::EMPTY; ICMP_PACKETS],
                vec![0u8; ICMP_BUFFER],
            ),
            raw::PacketBuffer::new(
                vec![raw::PacketMetadata::EMPTY; ICMP_PACKETS],
                vec![0u8; ICMP_BUFFER],
            ),
        ));
        Self {
            from_guest: Box::pin(from_guest),
            incoming: Vec::new(),
            timer: Timer::never(),
            deliver,
            frames,
            iface,
            sockets,
            connecting: HashMap::new(),
            tcp: HashMap::new(),
            tcp_keys: HashMap::new(),
            udp_ports: HashMap::new(),
            udp: HashMap::new(),
            icmp_socket,
            pings: HashMap::new(),
            dns_server: {
                let server = host_dns_server();
                log!("forwarding DNS to {server}");
                server
            },
            scratch: vec![0u8; MAX_DATAGRAM],
            reported: ReportOnce::new(),
        }
    }

    /// Where a connection to `destination` in the guest's world goes on the host.
    fn host_address(&self, destination: SocketAddrV4) -> SocketAddrV4 {
        let port = destination.port();
        match *destination.ip() {
            HOST_IP => SocketAddrV4::new(Ipv4Addr::LOCALHOST, port),
            DNS_IP => SocketAddrV4::new(self.dns_server, port),
            ip => SocketAddrV4::new(ip, port),
        }
    }

    async fn run(mut self) {
        loop {
            future::poll_fn(|cx| self.poll_events(cx)).await;
            let mut incoming = std::mem::take(&mut self.incoming).into_iter().peekable();
            loop {
                for frame in incoming.by_ref().take(BATCH) {
                    self.classify(frame);
                }
                self.iface
                    .poll(Instant::now(), &mut self.frames, &mut self.sockets);
                self.pump_tcp();
                self.pump_udp();
                self.pump_icmp();
                if incoming.peek().is_none() {
                    break;
                }
            }
            // Send what the pumps produced.
            self.iface
                .poll(Instant::now(), &mut self.frames, &mut self.sockets);
            if !self.frames.tx.is_empty() {
                (self.deliver)(std::mem::take(&mut self.frames.tx));
            }
        }
    }

    /// Waits for frames, connections, host sockets or smoltcp's timers. Frames go to
    /// `incoming`; finished connections release the guest's SYN.
    fn poll_events(&mut self, cx: &mut Context<'_>) -> Poll<()> {
        let mut ready = false;
        while let Poll::Ready(Some(frame)) = self.from_guest.poll_next(cx) {
            self.incoming.push(frame);
            ready = true;
        }
        let mut connected = Vec::new();
        for (key, connecting) in &mut self.connecting {
            if let Poll::Ready(result) = connecting.connect.as_mut().poll(cx) {
                connected.push((*key, result));
            }
        }
        for (key, result) in connected {
            self.connected(key, result);
            ready = true;
        }
        for (handle, flow) in &self.tcp {
            let socket = self.sockets.get::<tcp::Socket>(*handle);
            if !flow.host_eof && socket.can_send() && flow.stream.poll_readable(cx).is_ready() {
                ready = true;
            }
            if !flow.to_host.is_empty() && flow.stream.poll_writable(cx).is_ready() {
                ready = true;
            }
        }
        for flow in self.udp.values() {
            if flow.socket.poll_readable(cx).is_ready() {
                ready = true;
            }
        }
        for flow in self.pings.values() {
            if flow.socket.poll_readable(cx).is_ready() {
                ready = true;
            }
        }
        if ready {
            return Poll::Ready(());
        }
        let delay = self
            .iface
            .poll_delay(Instant::now(), &self.sockets)
            .map_or(MAX_WAIT, |delay| {
                Duration::from_micros(delay.total_micros()).min(MAX_WAIT)
            });
        self.timer.set_after(delay);
        Pin::new(&mut self.timer).poll(cx).map(|_| ())
    }

    /// Looks at a frame from the guest: holds back SYNs of new TCP connections and makes
    /// sure that UDP datagrams have a flow; the frames go to smoltcp.
    fn classify(&mut self, frame: Vec<u8>) {
        match peek(&frame) {
            Some(FlowStart::Syn(key)) if !self.tcp_keys.contains_key(&key) => {
                self.new_tcp_flow(key, frame);
                return;
            }
            Some(FlowStart::Udp((guest, destination))) => {
                let ip = destination.ip();
                // Nothing on the host would answer these.
                if ip.is_broadcast() || ip.is_multicast() || *ip == BROADCAST_IP {
                    return;
                }
                self.udp_flow(guest, destination);
            }
            _ => {}
        }
        self.frames.rx.push_back(frame);
    }

    /// Connects to the destination of a SYN on the host; the SYN waits for the outcome.
    fn new_tcp_flow(&mut self, key: FlowKey, syn: Vec<u8>) {
        if let Some(connecting) = self.connecting.get_mut(&key) {
            // The guest sent the SYN again.
            connecting.syn = syn;
            return;
        }
        let target = self.host_address(key.1);
        let connect = Box::pin(connect(SocketAddr::V4(target)));
        self.connecting.insert(key, PendingConnect { connect, syn });
    }

    /// The connection on the host for `key` was made (or not); smoltcp gets the SYN.
    fn connected(&mut self, key: FlowKey, result: io::Result<Async<TcpStream>>) {
        let Some(PendingConnect { syn, .. }) = self.connecting.remove(&key) else {
            return;
        };
        let stream = match result {
            Ok(stream) => stream,
            Err(error) => {
                // Without a listening socket, smoltcp answers the SYN with a reset (like a
                // refused connection), unless an ICMP error describes the failure.
                let reported = quote(&syn)
                    .is_some_and(|(header, data)| self.report_unreachable(&error, header, &data));
                if !reported {
                    self.receive_now(syn);
                }
                return;
            }
        };
        let _ = stream.get_ref().set_nodelay(true);
        let mut socket = tcp::Socket::new(
            tcp::SocketBuffer::new(vec![0u8; TCP_BUFFER]),
            tcp::SocketBuffer::new(vec![0u8; TCP_BUFFER]),
        );
        socket.set_timeout(Some(TCP_TIMEOUT.into()));
        if socket.listen(key.1).is_err() {
            self.receive_now(syn);
            return;
        }
        let handle = self.sockets.add(socket);
        self.receive_now(syn);
        // smoltcp may have dropped the SYN (e.g., because of a bad checksum); the guest
        // sends it again.
        if self.sockets.get::<tcp::Socket>(handle).state() != tcp::State::SynReceived {
            self.sockets.remove(handle);
            return;
        }
        self.tcp_keys.insert(key, handle);
        self.tcp.insert(
            handle,
            TcpFlow {
                key,
                stream,
                host_failed: false,
                to_host: Vec::new(),
                host_eof: false,
                shut_down_write: false,
            },
        );
    }

    /// Hands a frame to smoltcp right away. A listening socket accepts SYNs from any
    /// guest port, so smoltcp must get a connection's SYN before there is a socket for
    /// another connection to the same destination.
    fn receive_now(&mut self, frame: Vec<u8>) {
        self.frames.rx.push_back(frame);
        self.iface
            .poll(Instant::now(), &mut self.frames, &mut self.sockets);
    }

    fn pump_tcp(&mut self) {
        let mut finished = Vec::new();
        let buffer = &mut self.scratch;
        for (&handle, flow) in &mut self.tcp {
            let socket = self.sockets.get_mut::<tcp::Socket>(handle);
            let state = socket.state();
            match state {
                // The guest reset the connection before it was established (smoltcp
                // listens again).
                tcp::State::Listen => {
                    finished.push(handle);
                    continue;
                }
                // Not established yet (in particular, the guest cannot have closed its
                // side; may_recv() is false).
                tcp::State::SynReceived => continue,
                _ => {}
            }
            // Non-blocking I/O (Async made the socket non-blocking).
            let mut stream = flow.stream.get_ref();
            // Guest -> host, until the host takes no more (smoltcp keeps what it received
            // after the connection closed).
            while !flow.host_failed {
                while flow.to_host.len() < MAX_TO_HOST && socket.can_recv() {
                    match socket.recv_slice(buffer) {
                        Ok(0) | Err(_) => break,
                        Ok(count) => flow.to_host.extend_from_slice(&buffer[..count]),
                    }
                }
                if flow.to_host.is_empty() {
                    break;
                }
                match stream.write(&flow.to_host) {
                    Ok(count) => {
                        flow.to_host.drain(..count);
                    }
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
                    Err(_) => {
                        // smoltcp sends the reset while the socket still exists.
                        socket.abort();
                        flow.host_failed = true;
                        flow.to_host.clear();
                    }
                }
            }
            if !flow.host_failed
                && !socket.may_recv()
                && flow.to_host.is_empty()
                && !flow.shut_down_write
            {
                let _ = stream.shutdown(Shutdown::Write);
                flow.shut_down_write = true;
            }
            // Host -> guest.
            while !flow.host_eof && !flow.host_failed && socket.can_send() {
                let room = (socket.send_capacity() - socket.send_queue()).min(buffer.len());
                if room == 0 {
                    break;
                }
                match stream.read(&mut buffer[..room]) {
                    Ok(0) => {
                        flow.host_eof = true;
                        socket.close();
                    }
                    Ok(count) => {
                        let _ = socket.send_slice(&buffer[..count]);
                    }
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
                    Err(_) => {
                        // smoltcp sends the reset while the socket still exists.
                        socket.abort();
                        flow.host_failed = true;
                        flow.to_host.clear();
                    }
                }
            }
            // smoltcp closes the socket after TIME-WAIT, after a reset or a timeout, and
            // when we abort it (then, once it sent the reset). What the guest sent must
            // reach the host first.
            if state == tcp::State::Closed
                && (flow.host_failed || (flow.to_host.is_empty() && !socket.can_recv()))
            {
                finished.push(handle);
            }
        }
        for handle in finished {
            if let Some(flow) = self.tcp.remove(&handle) {
                self.tcp_keys.remove(&flow.key);
                self.sockets.remove(handle);
            }
        }
    }

    /// Makes sure that the guest's datagrams from `guest` to `destination` have a flow
    /// (and that smoltcp has a socket for the destination port).
    fn udp_flow(&mut self, guest: SocketAddrV4, destination: SocketAddrV4) {
        let port = destination.port();
        if !self.udp_ports.contains_key(&port) {
            let mut socket = udp::Socket::new(
                udp::PacketBuffer::new(
                    vec![udp::PacketMetadata::EMPTY; UDP_PACKETS],
                    vec![0u8; UDP_BUFFER],
                ),
                udp::PacketBuffer::new(
                    vec![udp::PacketMetadata::EMPTY; UDP_PACKETS],
                    vec![0u8; UDP_BUFFER],
                ),
            );
            if socket.bind(port).is_err() {
                return;
            }
            self.udp_ports.insert(port, self.sockets.add(socket));
        }
        if self.udp.contains_key(&(guest, destination)) {
            return;
        }
        let target = self.host_address(destination);
        match Async::<UdpSocket>::bind(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, 0)) {
            Ok(socket) => {
                let flow = UdpFlow {
                    socket,
                    target,
                    last_used: std::time::Instant::now(),
                };
                self.udp.insert((guest, destination), flow);
            }
            // smoltcp still receives the datagram, which nobody takes.
            Err(error) => {
                report!(
                    self.reported,
                    Problem::UdpSocket,
                    "cannot forward UDP to {target}: {error}"
                );
            }
        }
    }

    fn pump_udp(&mut self) {
        let now = std::time::Instant::now();
        // Guest -> host.
        let mut failed = Vec::new();
        for (&port, &handle) in &self.udp_ports {
            let socket = self.sockets.get_mut::<udp::Socket>(handle);
            while let Ok((payload, meta)) = socket.recv() {
                let Some(local) = meta.local_address else {
                    continue;
                };
                let guest = SocketAddrV4::new(ipv4(meta.endpoint.addr), meta.endpoint.port);
                let destination = SocketAddrV4::new(ipv4(local), port);
                let Some(flow) = self.udp.get_mut(&(guest, destination)) else {
                    continue;
                };
                flow.last_used = now;
                match flow.socket.get_ref().send_to(payload, flow.target) {
                    Ok(_) => {}
                    // Without room in the host's buffer, the datagram is lost (like on a busy
                    // link).
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
                    Err(error) => failed.push((guest, destination, payload.len(), error)),
                }
            }
        }
        for (guest, destination, len, error) in failed {
            let length = (8 + len) as u16;
            let header = Ipv4Repr {
                src_addr: *guest.ip(),
                dst_addr: *destination.ip(),
                next_header: IpProtocol::Udp,
                payload_len: length.into(),
                hop_limit: 64,
            };
            // The UDP header (without a checksum).
            let mut data = [0u8; QUOTE_LEN];
            data[0..2].copy_from_slice(&guest.port().to_be_bytes());
            data[2..4].copy_from_slice(&destination.port().to_be_bytes());
            data[4..6].copy_from_slice(&length.to_be_bytes());
            if !self.report_unreachable(&error, header, &data) {
                report!(
                    self.reported,
                    Problem::SendDatagram,
                    "sending UDP to {destination}: {error}"
                );
            }
        }
        // Host -> guest.
        for (&(guest, destination), flow) in &mut self.udp {
            let Some(&handle) = self.udp_ports.get(&destination.port()) else {
                continue;
            };
            let socket = self.sockets.get_mut::<udp::Socket>(handle);
            // While smoltcp has room (the rest waits in the host's socket).
            while socket.can_send() {
                match flow.socket.get_ref().recv_from(&mut self.scratch) {
                    // Only replies from the target.
                    Ok((_, source)) if source != SocketAddr::V4(flow.target) => {}
                    Ok((count, _)) => {
                        flow.last_used = now;
                        let mut meta = udp::UdpMetadata::from(IpEndpoint::from(guest));
                        meta.local_address = Some(IpAddress::Ipv4(*destination.ip()));
                        // If the datagram does not fit, it is lost (like on a busy link).
                        let _ = socket.send_slice(&self.scratch[..count], meta);
                    }
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
                    Err(error) => {
                        report!(
                            self.reported,
                            Problem::ReceiveDatagram,
                            "receiving UDP from {destination}: {error}"
                        );
                        break;
                    }
                }
            }
        }
        self.udp.retain(|(_, destination), flow| {
            let idle = if destination.port() == DNS_PORT {
                DNS_IDLE
            } else {
                FLOW_IDLE
            };
            now.duration_since(flow.last_used) < idle
        });
        let ports: HashSet<u16> = self.udp.keys().map(|(_, d)| d.port()).collect();
        self.udp_ports.retain(|port, handle| {
            let used = ports.contains(port);
            if !used {
                self.sockets.remove(*handle);
            }
            used
        });
    }

    /// Sends an ICMP message to the guest (smoltcp fills in the Ethernet header and
    /// fragments it if necessary).
    fn send_icmp(&mut self, source: Ipv4Addr, guest: Ipv4Addr, icmp: &Icmpv4Repr) {
        let packet = icmp_packet(source, guest, icmp);
        // Without room in smoltcp's buffer, the message is lost.
        let _ = self
            .sockets
            .get_mut::<raw::Socket>(self.icmp_socket)
            .send_slice(&packet);
    }

    fn pump_icmp(&mut self) {
        let now = std::time::Instant::now();
        // Guest -> host.
        let mut requests = Vec::new();
        let socket = self.sockets.get_mut::<raw::Socket>(self.icmp_socket);
        while let Ok(packet) = socket.recv() {
            requests.extend(parse_echo_request(packet));
        }
        for request in requests {
            self.ping(request, now);
        }
        // Host -> guest.
        let mut replies = Vec::new();
        for (&(ident, destination), flow) in &mut self.pings {
            loop {
                match flow.socket.recv(&mut self.scratch) {
                    Ok(Some(reply)) if reply.source == destination => {
                        flow.last_used = now;
                        replies.push((destination, flow.guest, ident, reply));
                    }
                    Ok(_) => {}
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
                    Err(error) => {
                        report!(
                            self.reported,
                            Problem::ReceivePing,
                            "receiving ICMP from {destination}: {error}"
                        );
                        break;
                    }
                }
            }
        }
        for (destination, guest, ident, reply) in replies {
            let icmp = Icmpv4Repr::EchoReply {
                ident,
                seq_no: reply.seq_no,
                data: &reply.data,
            };
            self.send_icmp(destination, guest, &icmp);
        }
        self.pings
            .retain(|_, flow| now.duration_since(flow.last_used) < FLOW_IDLE);
    }

    /// Answers an echo request of the guest, or forwards it to its destination.
    fn ping(&mut self, request: EchoRequest, now: std::time::Instant) {
        let EchoRequest {
            guest,
            destination,
            ident,
            seq_no,
            ref data,
        } = request;
        if matches!(destination, HOST_IP | DNS_IP) {
            let icmp = Icmpv4Repr::EchoReply {
                ident,
                seq_no,
                data,
            };
            self.send_icmp(destination, guest, &icmp);
            return;
        }
        let key = (ident, destination);
        if !self.pings.contains_key(&key) {
            let socket = match PingSocket::new() {
                Ok(socket) => socket,
                Err(error) => {
                    report!(
                        self.reported,
                        Problem::PingSocket,
                        "cannot forward pings: {error}"
                    );
                    return;
                }
            };
            let flow = PingFlow {
                socket,
                guest,
                last_used: now,
            };
            self.pings.insert(key, flow);
        }
        let flow = self.pings.get_mut(&key).unwrap();
        flow.last_used = now;
        if let Err(error) = flow.socket.send(destination, ident, seq_no, data) {
            let icmp = Icmpv4Repr::EchoRequest {
                ident,
                seq_no,
                data,
            };
            let header = Ipv4Repr {
                src_addr: guest,
                dst_addr: destination,
                next_header: IpProtocol::Icmp,
                payload_len: icmp.buffer_len(),
                hop_limit: 64,
            };
            let mut message = vec![0u8; icmp.buffer_len()];
            icmp.emit(
                &mut Icmpv4Packet::new_unchecked(&mut message[..]),
                &ChecksumCapabilities::default(),
            );
            // Otherwise, the guest sees no reply.
            self.report_unreachable(&error, header, &message[..QUOTE_LEN]);
        }
    }

    /// Tells the guest that the host could not deliver its packet (`header` and the start
    /// of its payload, `data`) because of `error`, if an ICMP error describes it. Returns
    /// whether it did.
    fn report_unreachable(&mut self, error: &io::Error, header: Ipv4Repr, data: &[u8]) -> bool {
        // A router reports unreachable networks and hosts; the destination reports closed
        // ports.
        let (reason, source) = match error.kind() {
            io::ErrorKind::NetworkUnreachable => (Icmpv4DstUnreachable::NetUnreachable, HOST_IP),
            io::ErrorKind::HostUnreachable | io::ErrorKind::TimedOut => {
                (Icmpv4DstUnreachable::HostUnreachable, HOST_IP)
            }
            io::ErrorKind::ConnectionRefused if header.next_header == IpProtocol::Udp => {
                (Icmpv4DstUnreachable::PortUnreachable, header.dst_addr)
            }
            _ => return false,
        };
        let icmp = Icmpv4Repr::DstUnreachable {
            reason,
            header,
            data: &data[..data.len().min(QUOTE_LEN)],
        };
        self.send_icmp(source, header.src_addr, &icmp);
        true
    }
}

#[cfg(test)]
mod tests {
    use std::net::{TcpListener, UdpSocket};
    use std::os::fd::AsRawFd;
    use std::sync::mpsc;

    use smoltcp::socket::tcp;

    use smoltcp::wire::{ArpOperation, ArpPacket, ArpRepr, EthernetRepr, Icmpv4Message};

    use super::*;

    /// A guest made of a smoltcp interface, connected to a [`UserNet`] that runs on its own
    /// thread.
    struct Guest {
        iface: Interface,
        frames: Frames,
        sockets: SocketSet<'static>,
        handle: Arc<UserNetHandle>,
        to_guest: mpsc::Receiver<Vec<Vec<u8>>>,
        /// Stops the stack's thread when the guest is dropped.
        _stop: async_channel::Sender<()>,
    }

    impl Guest {
        fn new() -> Self {
            let (handle, from_guest) = channel();
            let (sender, to_guest) = mpsc::channel();
            let (stop, stopped) = async_channel::bounded::<()>(1);
            std::thread::spawn(move || {
                let executor = Executor::new();
                spawn(&executor, from_guest, move |frames| {
                    let _ = sender.send(frames);
                });
                let _ = async_io::block_on(executor.run(stopped.recv()));
            });
            let mut frames = Frames::default();
            let config = Config::new(HardwareAddress::Ethernet(EthernetAddress(GUEST_MAC)));
            let mut iface = Interface::new(config, &mut frames, Instant::now());
            iface.update_ip_addrs(|addresses| {
                let _ = addresses.push(IpCidr::new(IpAddress::Ipv4(GUEST_IP), 24));
            });
            let _ = iface.routes_mut().add_default_ipv4_route(HOST_IP);
            Self {
                iface,
                frames,
                sockets: SocketSet::new(Vec::new()),
                handle,
                to_guest,
                _stop: stop,
            }
        }

        /// Exchanges frames with the stack until `done` returns true.
        fn run_until(&mut self, mut done: impl FnMut(&mut Self) -> bool) {
            let deadline = std::time::Instant::now() + Duration::from_secs(10);
            while !done(self) {
                assert!(std::time::Instant::now() < deadline, "timed out");
                self.iface
                    .poll(Instant::now(), &mut self.frames, &mut self.sockets);
                for frame in self.frames.tx.drain(..) {
                    self.handle.send_from_guest(frame);
                }
                if let Ok(frames) = self.to_guest.recv_timeout(Duration::from_millis(5)) {
                    self.frames.rx.extend(frames);
                }
            }
        }

        fn udp_socket(&mut self, port: u16) -> SocketHandle {
            let buffer = || {
                udp::PacketBuffer::new(
                    vec![udp::PacketMetadata::EMPTY; 8],
                    vec![0u8; 4 * MAX_DATAGRAM],
                )
            };
            let mut socket = udp::Socket::new(buffer(), buffer());
            socket.bind(port).unwrap();
            self.sockets.add(socket)
        }

        fn tcp_socket(&mut self) -> SocketHandle {
            let buffer = || tcp::SocketBuffer::new(vec![0u8; TCP_BUFFER]);
            self.sockets.add(tcp::Socket::new(buffer(), buffer()))
        }

        fn icmp_socket(&mut self) -> SocketHandle {
            let buffer =
                || raw::PacketBuffer::new(vec![raw::PacketMetadata::EMPTY; 8], vec![0u8; 4096]);
            self.sockets.add(raw::Socket::new(
                Some(IpVersion::Ipv4),
                Some(IpProtocol::Icmp),
                buffer(),
                buffer(),
            ))
        }
    }

    /// Pings `destination` from the guest and checks the reply.
    fn ping(destination: Ipv4Addr) {
        let mut guest = Guest::new();
        let handle = guest.icmp_socket();
        let request = Icmpv4Repr::EchoRequest {
            ident: 0x1234,
            seq_no: 7,
            data: b"data",
        };
        guest
            .sockets
            .get_mut::<raw::Socket>(handle)
            .send_slice(&icmp_packet(GUEST_IP, destination, &request))
            .unwrap();
        guest.run_until(|guest| {
            let socket = guest.sockets.get_mut::<raw::Socket>(handle);
            let Ok(packet) = socket.recv() else {
                return false;
            };
            let ip = Ipv4Packet::new_checked(packet).unwrap();
            let icmp = Icmpv4Packet::new_checked(ip.payload()).unwrap();
            let reply = Icmpv4Repr::parse(&icmp, &ChecksumCapabilities::default()).unwrap();
            assert_eq!(ip.src_addr(), destination);
            assert_eq!(
                reply,
                Icmpv4Repr::EchoReply {
                    ident: 0x1234,
                    seq_no: 7,
                    data: b"data",
                }
            );
            true
        });
    }

    #[test]
    fn unreachable_destinations() {
        let (_handle, from_guest) = channel();
        let mut stack = UserNet::new(from_guest, Box::new(|_| {}));
        // smoltcp learns the guest's MAC address from its ARP request.
        let arp = ArpRepr::EthernetIpv4 {
            operation: ArpOperation::Request,
            source_hardware_addr: EthernetAddress(GUEST_MAC),
            source_protocol_addr: GUEST_IP,
            target_hardware_addr: EthernetAddress([0; 6]),
            target_protocol_addr: HOST_IP,
        };
        let ethernet = EthernetRepr {
            src_addr: EthernetAddress(GUEST_MAC),
            dst_addr: EthernetAddress::BROADCAST,
            ethertype: EthernetProtocol::Arp,
        };
        let mut frame = vec![0u8; ethernet.buffer_len() + arp.buffer_len()];
        let mut ethernet_frame = EthernetFrame::new_unchecked(&mut frame[..]);
        ethernet.emit(&mut ethernet_frame);
        arp.emit(&mut ArpPacket::new_unchecked(ethernet_frame.payload_mut()));
        stack.frames.rx.push_back(frame);

        let destination = Ipv4Addr::new(192, 0, 2, 1);
        let header = Ipv4Repr {
            src_addr: GUEST_IP,
            dst_addr: destination,
            next_header: IpProtocol::Udp,
            payload_len: 108,
            hop_limit: 64,
        };
        let data = [0x0f, 0xa0, 0x00, 0x35, 0x00, 0x6c, 0x00, 0x00];
        let denied = io::Error::from(io::ErrorKind::PermissionDenied);
        assert!(!stack.report_unreachable(&denied, header, &data));
        let unreachable = io::Error::from(io::ErrorKind::HostUnreachable);
        assert!(stack.report_unreachable(&unreachable, header, &data));
        stack
            .iface
            .poll(Instant::now(), &mut stack.frames, &mut stack.sockets);

        let messages: Vec<_> = (stack.frames.tx.iter())
            .filter_map(|frame| {
                let ethernet = EthernetFrame::new_checked(&frame[..]).ok()?;
                let ip = Ipv4Packet::new_checked(ethernet.payload()).ok()?;
                (ip.next_header() == IpProtocol::Icmp).then(|| {
                    assert_eq!((ip.src_addr(), ip.dst_addr()), (HOST_IP, GUEST_IP));
                    // (smoltcp cannot parse the message: its quote of the guest's
                    // packet is shorter than the packet.)
                    let icmp = Icmpv4Packet::new_checked(ip.payload()).unwrap();
                    assert_eq!(icmp.msg_type(), Icmpv4Message::DstUnreachable);
                    let reason = Icmpv4DstUnreachable::HostUnreachable;
                    assert_eq!(icmp.msg_code(), u8::from(reason));
                    assert!(icmp.verify_checksum());
                    let quote = Ipv4Packet::new_unchecked(icmp.data());
                    assert_eq!(quote.src_addr(), GUEST_IP);
                    assert_eq!(quote.dst_addr(), destination);
                    assert_eq!(quote.total_len(), 128);
                    assert_eq!(&icmp.data()[quote.header_len().into()..], data);
                })
            })
            .collect();
        assert_eq!(messages.len(), 1);
    }

    #[test]
    fn ping_the_gateway() {
        ping(HOST_IP);
    }

    #[test]
    fn forwarded_ping() {
        // The host may not let us create ping sockets (on Linux, see
        // net.ipv4.ping_group_range).
        if PingSocket::new().is_err() {
            eprintln!("skipped: no ping sockets");
            return;
        }
        ping(Ipv4Addr::LOCALHOST);
    }

    /// Exchanges a datagram of `size` bytes with a UDP socket on the host's loopback (as
    /// 10.0.2.2) in both directions.
    fn udp_round_trip(size: usize) {
        let host = UdpSocket::bind("127.0.0.1:0").unwrap();
        let destination = (HOST_IP, host.local_addr().unwrap().port());
        let data: Vec<u8> = (0..size).map(|i| i as u8).collect();
        let mut guest = Guest::new();
        let handle = guest.udp_socket(4000);
        guest
            .sockets
            .get_mut::<udp::Socket>(handle)
            .send_slice(&data, destination)
            .unwrap();

        let mut received = vec![0u8; MAX_DATAGRAM];
        host.set_nonblocking(true).unwrap();
        let mut from = None;
        guest.run_until(|_| match host.recv_from(&mut received) {
            Ok((count, source)) => {
                assert_eq!(&received[..count], data);
                from = Some(source);
                true
            }
            Err(_) => false,
        });
        host.send_to(&data, from.unwrap()).unwrap();
        guest.run_until(|guest| {
            let socket = guest.sockets.get_mut::<udp::Socket>(handle);
            match socket.recv() {
                Ok((payload, meta)) => {
                    assert_eq!(payload, data);
                    assert_eq!(meta.endpoint, IpEndpoint::from(destination));
                    true
                }
                Err(_) => false,
            }
        });
    }

    #[test]
    fn udp() {
        udp_round_trip(100);
    }

    #[test]
    fn fragmented_udp() {
        udp_round_trip(20000);
    }

    #[test]
    fn tcp_connection() {
        let host = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = host.local_addr().unwrap().port();
        let mut guest = Guest::new();
        let handle = tcp_connect(&mut guest, port, 4000);
        let (mut stream, _) = host.accept().unwrap();

        guest
            .sockets
            .get_mut::<tcp::Socket>(handle)
            .send_slice(b"ping")
            .unwrap();
        let mut request = [0u8; 4];
        stream.set_nonblocking(true).unwrap();
        guest.run_until(|_| stream.read_exact(&mut request).is_ok());
        assert_eq!(&request, b"ping");

        stream.write_all(b"pong").unwrap();
        drop(stream);
        let mut reply = Vec::new();
        guest.run_until(|guest| {
            let socket = guest.sockets.get_mut::<tcp::Socket>(handle);
            while let Ok(data) = socket.recv(|data| (data.len(), data.to_vec())) {
                if data.is_empty() {
                    break;
                }
                reply.extend(data);
            }
            !socket.may_recv()
        });
        assert_eq!(reply, b"pong");
    }

    /// Connects a guest socket to `port` on the host's loopback (as 10.0.2.2).
    fn tcp_connect(guest: &mut Guest, port: u16, local_port: u16) -> SocketHandle {
        let handle = guest.tcp_socket();
        let context = guest.iface.context();
        guest
            .sockets
            .get_mut::<tcp::Socket>(handle)
            .connect(context, (HOST_IP, port), local_port)
            .unwrap();
        guest.run_until(|guest| guest.sockets.get::<tcp::Socket>(handle).may_send());
        handle
    }

    #[test]
    fn parallel_tcp_connections() {
        let host = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = host.local_addr().unwrap().port();
        let mut guest = Guest::new();
        let handles = [4000, 4001].map(|local_port| tcp_connect(&mut guest, port, local_port));
        for (handle, data) in handles.iter().zip([b"first", b"other"]) {
            let socket = guest.sockets.get_mut::<tcp::Socket>(*handle);
            socket.send_slice(data).unwrap();
        }
        // Each connection on the host gets the data of one of the guest's.
        let mut received = Vec::new();
        for _ in 0..2 {
            let (mut stream, _) = host.accept().unwrap();
            stream.set_nonblocking(true).unwrap();
            let mut data = [0u8; 5];
            guest.run_until(|_| stream.read_exact(&mut data).is_ok());
            received.push(data);
        }
        received.sort();
        assert_eq!(received, [*b"first", *b"other"]);
    }

    #[test]
    fn reset_by_the_host() {
        let host = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = host.local_addr().unwrap().port();
        let mut guest = Guest::new();
        let handle = tcp_connect(&mut guest, port, 4000);
        let (stream, _) = host.accept().unwrap();
        // Closing with a zero linger time resets the connection.
        let linger = libc::linger {
            l_onoff: 1,
            l_linger: 0,
        };
        // SAFETY: setsockopt() reads the option from a valid pointer.
        let result = unsafe {
            libc::setsockopt(
                stream.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_LINGER,
                (&raw const linger).cast(),
                size_of::<libc::linger>() as libc::socklen_t,
            )
        };
        assert_eq!(result, 0);
        drop(stream);
        // The guest's connection is reset as well (it does not see an orderly close).
        guest.run_until(|guest| {
            let state = guest.sockets.get::<tcp::Socket>(handle).state();
            assert_ne!(state, tcp::State::CloseWait, "the guest saw a FIN");
            state == tcp::State::Closed
        });
    }

    #[test]
    fn refused_tcp_connection() {
        // A port that nobody listens on (once the listener is gone).
        let port = TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let mut guest = Guest::new();
        let handle = guest.tcp_socket();
        let context = guest.iface.context();
        guest
            .sockets
            .get_mut::<tcp::Socket>(handle)
            .connect(context, (HOST_IP, port), 4000)
            .unwrap();
        // The guest sees a reset instead of an established connection.
        let mut established = false;
        guest.run_until(|guest| {
            let socket = guest.sockets.get::<tcp::Socket>(handle);
            established |= socket.may_send();
            socket.state() == tcp::State::Closed
        });
        assert!(!established);
    }
}
