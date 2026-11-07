//! Unprivileged ICMP sockets of the host ("ping sockets"), through which the network
//! stack forwards the guest's pings. On Linux, only the groups in
//! net.ipv4.ping_group_range may create them.

use std::io;
use std::mem::size_of;
use std::net::Ipv4Addr;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::task::{Context, Poll};

use async_io::Async;
use smoltcp::phy::ChecksumCapabilities;
use smoltcp::wire::{Icmpv4Message, Icmpv4Packet, Icmpv4Repr, Ipv4Packet};

/// An echo reply that a [`PingSocket`] received.
pub(crate) struct EchoReply {
    pub source: Ipv4Addr,
    pub seq_no: u16,
    pub data: Vec<u8>,
}

pub(crate) struct PingSocket {
    fd: Async<OwnedFd>,
    /// The identifier of our echo requests as the host sends them. Linux replaces it with
    /// its own (the socket's "port"), managarm keeps ours.
    ident: Option<u16>,
}

fn sockaddr(address: Ipv4Addr) -> libc::sockaddr_in {
    // SAFETY: sockaddr_in is plain old data; all zeros is a valid value.
    let mut sockaddr: libc::sockaddr_in = unsafe { std::mem::zeroed() };
    sockaddr.sin_family = libc::AF_INET as libc::sa_family_t;
    sockaddr.sin_addr.s_addr = u32::from(address).to_be();
    sockaddr
}

/// Returns -1 (as a C function does) as the last OS error.
fn check(result: isize) -> io::Result<usize> {
    if result < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(result as usize)
    }
}

/// The echo reply in a message that a ping socket received: its identifier, its sequence
/// number and its data.
fn parse_echo_reply(message: &[u8]) -> Option<(u16, u16, &[u8])> {
    // Some hosts pass on the IP header (Linux and managarm do not); it starts with the
    // version (4), an echo reply with its type (0).
    let message = if message.first().is_some_and(|byte| byte >> 4 == 4) {
        Ipv4Packet::new_checked(message).ok()?.payload()
    } else {
        message
    };
    let icmp = Icmpv4Packet::new_checked(message).ok()?;
    (icmp.msg_type() == Icmpv4Message::EchoReply && icmp.msg_code() == 0)
        .then(|| (icmp.echo_ident(), icmp.echo_seq_no(), icmp.data()))
}

impl PingSocket {
    pub(crate) fn new() -> io::Result<Self> {
        // SAFETY: socket() has no preconditions.
        let fd = unsafe {
            libc::socket(
                libc::AF_INET,
                libc::SOCK_DGRAM | libc::SOCK_CLOEXEC,
                libc::IPPROTO_ICMP,
            )
        };
        check(fd as isize)?;
        // SAFETY: socket() returned a new file descriptor that nothing else owns.
        let fd = Async::new(unsafe { OwnedFd::from_raw_fd(fd) })?;
        Ok(Self { fd, ident: None })
    }

    /// Sends an echo request to `destination`.
    pub(crate) fn send(
        &mut self,
        destination: Ipv4Addr,
        ident: u16,
        seq_no: u16,
        data: &[u8],
    ) -> io::Result<()> {
        let repr = Icmpv4Repr::EchoRequest {
            ident,
            seq_no,
            data,
        };
        let mut message = vec![0u8; repr.buffer_len()];
        repr.emit(
            &mut Icmpv4Packet::new_unchecked(&mut message[..]),
            &ChecksumCapabilities::default(),
        );
        let address = sockaddr(destination);
        // SAFETY: The message and the address are valid for the call. No flags: managarm
        // does not support any on ICMP sockets.
        check(unsafe {
            libc::sendto(
                self.fd.as_raw_fd(),
                message.as_ptr().cast(),
                message.len(),
                0,
                (&raw const address).cast(),
                size_of::<libc::sockaddr_in>() as libc::socklen_t,
            )
        })?;
        if self.ident.is_none() {
            self.ident = Some(self.local_ident().unwrap_or(ident));
        }
        Ok(())
    }

    /// The identifier that the host chose for our requests (Linux binds the socket to it
    /// when it sends the first one), if it chose one.
    fn local_ident(&self) -> Option<u16> {
        let mut address = sockaddr(Ipv4Addr::UNSPECIFIED);
        let mut length = size_of::<libc::sockaddr_in>() as libc::socklen_t;
        // SAFETY: getsockname() writes at most `length` bytes of the address.
        let result = unsafe {
            libc::getsockname(self.fd.as_raw_fd(), (&raw mut address).cast(), &mut length)
        };
        let port = u16::from_be(address.sin_port);
        (result == 0 && port != 0).then_some(port)
    }

    /// Receives the next echo reply to our requests (with `buffer` as scratch space);
    /// returns `Ok(None)` for other messages and fails with `WouldBlock` once there is
    /// nothing to receive.
    pub(crate) fn recv(&self, buffer: &mut [u8]) -> io::Result<Option<EchoReply>> {
        let mut address = sockaddr(Ipv4Addr::UNSPECIFIED);
        let mut length = size_of::<libc::sockaddr_in>() as libc::socklen_t;
        // SAFETY: recvfrom() writes at most the sizes that we pass.
        let count = check(unsafe {
            libc::recvfrom(
                self.fd.as_raw_fd(),
                buffer.as_mut_ptr().cast(),
                buffer.len(),
                libc::MSG_DONTWAIT,
                (&raw mut address).cast(),
                &mut length,
            )
        })?;
        let source = Ipv4Addr::from(u32::from_be(address.sin_addr.s_addr));
        // managarm's ICMP sockets receive all ICMP messages, not only our replies.
        Ok(parse_echo_reply(&buffer[..count])
            .filter(|(ident, _, _)| Some(*ident) == self.ident)
            .map(|(_, seq_no, data)| EchoReply {
                source,
                seq_no,
                data: data.to_vec(),
            }))
    }

    pub(crate) fn poll_readable(&self, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.fd.poll_readable(cx)
    }
}

#[cfg(test)]
mod tests {
    use smoltcp::wire::{IpProtocol, Ipv4Repr};

    use super::*;

    fn echo_reply() -> Vec<u8> {
        let repr = Icmpv4Repr::EchoReply {
            ident: 1,
            seq_no: 2,
            data: b"data",
        };
        let mut message = vec![0u8; repr.buffer_len()];
        repr.emit(
            &mut Icmpv4Packet::new_unchecked(&mut message[..]),
            &ChecksumCapabilities::default(),
        );
        message
    }

    #[test]
    fn echo_replies() {
        let message = echo_reply();
        assert_eq!(parse_echo_reply(&message), Some((1, 2, &b"data"[..])));

        // With the IP header.
        let ip = Ipv4Repr {
            src_addr: Ipv4Addr::LOCALHOST,
            dst_addr: Ipv4Addr::LOCALHOST,
            next_header: IpProtocol::Icmp,
            payload_len: message.len(),
            hop_limit: 64,
        };
        let mut packet = vec![0u8; ip.buffer_len() + message.len()];
        let mut ip_packet = Ipv4Packet::new_unchecked(&mut packet[..]);
        ip.emit(&mut ip_packet, &ChecksumCapabilities::default());
        ip_packet.payload_mut().copy_from_slice(&message);
        assert_eq!(parse_echo_reply(&packet), Some((1, 2, &b"data"[..])));

        // Not an echo reply.
        let mut request = message.clone();
        Icmpv4Packet::new_unchecked(&mut request[..]).set_msg_type(Icmpv4Message::EchoRequest);
        assert_eq!(parse_echo_reply(&request), None);
    }
}
