//! Minimal DHCPv4 server for the setup access point (ADR 0004, component 3).
//!
//! The server owns 8 addresses, 192.168.4.10 .. 192.168.4.17, and hands them
//! to whatever phone or laptop joins `wfi028t-setup-XXXX`. It answers
//! DISCOVER with an OFFER, REQUEST with an ACK (or a NAK for an address that
//! is not ours or was never offered to that client), frees a lease on
//! RELEASE, and ignores every other message type. No relay support, no
//! options beyond the ones the ADR lists.
//!
//! What the caller does with sockets:
//!
//! - bind one UDP socket on the AP interface to `0.0.0.0:67`
//!   ([`SERVER_PORT`]) and accept broadcasts,
//! - for every datagram, call [`DhcpServer::handle`] with the payload, the
//!   current time in seconds (any monotonic clock; only differences matter)
//!   and a buffer of at least [`MAX_REPLY`] bytes,
//! - on `Some(reply)`, send `out[..reply.len]` to UDP port [`CLIENT_PORT`] at
//!   the address in [`Reply::to`]; on `None`, send nothing.
//!
//! [`Destination::ClientNoArp`] is a unicast to a client that does not have
//! the address yet, so it needs a static ARP entry. A stack that cannot add
//! one (embassy-net cannot) must broadcast that reply instead - every DHCP
//! client accepts it, because the reply carries its own xid and MAC.
//!
//! The lease table is keyed by the client MAC (`chaddr`), so a client that
//! comes back gets the same address. Expired leases are evicted when the
//! table is full; while all 8 are live, a new client gets no answer.

/// The address of the setup access point, and the router and DNS server we
/// hand out.
pub const SERVER_IP: [u8; 4] = [192, 168, 4, 1];
/// /24, as the ADR specifies.
pub const SUBNET_MASK: [u8; 4] = [255, 255, 255, 0];
/// First address of the pool; the pool is [`POOL_SIZE`] addresses from here.
pub const POOL_START: [u8; 4] = [192, 168, 4, 10];
/// Number of addresses the server can hand out at once.
pub const POOL_SIZE: usize = 8;
/// Lease time in seconds (one hour).
pub const LEASE_SECS: u32 = 3600;
/// UDP port the server listens on.
pub const SERVER_PORT: u16 = 67;
/// UDP port replies go to.
pub const CLIENT_PORT: u16 = 68;
/// Every reply is exactly this long: the BOOTP frame plus options, padded
/// with zeros to the 300 bytes old clients expect.
pub const MAX_REPLY: usize = 300;

// BOOTP / DHCP wire layout.
const OP: usize = 0;
const HTYPE: usize = 1;
const HLEN: usize = 2;
const XID: usize = 4;
const FLAGS: usize = 10;
const CIADDR: usize = 12;
const YIADDR: usize = 16;
const GIADDR: usize = 24;
const CHADDR: usize = 28;
const COOKIE: usize = 236;
const OPTIONS: usize = 240;

const BOOTREQUEST: u8 = 1;
const BOOTREPLY: u8 = 2;
const HTYPE_ETHER: u8 = 1;
const MAC_LEN: usize = 6;
const MAGIC: [u8; 4] = [0x63, 0x82, 0x53, 0x63];
const FLAG_BROADCAST: u16 = 0x8000;

const DISCOVER: u8 = 1;
const OFFER: u8 = 2;
const REQUEST: u8 = 3;
const ACK: u8 = 5;
const NAK: u8 = 6;
const RELEASE: u8 = 7;

const OPT_PAD: u8 = 0;
const OPT_SUBNET: u8 = 1;
const OPT_ROUTER: u8 = 3;
const OPT_DNS: u8 = 6;
const OPT_REQUESTED_IP: u8 = 50;
const OPT_LEASE_TIME: u8 = 51;
const OPT_MSG_TYPE: u8 = 53;
const OPT_SERVER_ID: u8 = 54;
const OPT_END: u8 = 255;

/// Where a reply has to go. The caller sends it; this crate has no sockets.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Destination {
    /// 255.255.255.255: the client asked for a broadcast, or it is a NAK.
    Broadcast,
    /// The client already has this address configured (`ciaddr` was set), so
    /// a plain unicast reaches it.
    Client {
        /// Address to send to.
        ip: [u8; 4],
    },
    /// The client does not have the address yet: a unicast needs an ARP entry
    /// for `mac`. Broadcast instead if the stack cannot add one.
    ClientNoArp {
        /// Address we are about to give the client.
        ip: [u8; 4],
        /// Hardware address from `chaddr`.
        mac: [u8; MAC_LEN],
    },
}

/// A reply to send: `out[..len]` to [`CLIENT_PORT`] at `to`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Reply {
    /// Bytes written at the start of the caller's buffer.
    pub len: usize,
    /// Where to send them.
    pub to: Destination,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum LeaseState {
    Free,
    /// Offered, not confirmed yet. A short reservation so two clients that
    /// discover at the same time do not get the same address.
    Offered,
    /// Confirmed by a REQUEST.
    Bound,
}

#[derive(Clone, Copy)]
struct Lease {
    mac: [u8; MAC_LEN],
    /// Seconds value (as passed to [`DhcpServer::handle`]) at which this
    /// lease or offer stops counting as live.
    expires: u32,
    state: LeaseState,
}

impl Lease {
    const FREE: Self = Self {
        mac: [0; MAC_LEN],
        expires: 0,
        state: LeaseState::Free,
    };

    fn live(&self, now: u32) -> bool {
        self.state != LeaseState::Free && self.expires > now
    }
}

/// An offer is held for this long if the client never sends its REQUEST.
const OFFER_SECS: u32 = 30;

/// The lease table. One per access point; cheap to keep in a static.
pub struct DhcpServer {
    leases: [Lease; POOL_SIZE],
}

impl Default for DhcpServer {
    fn default() -> Self {
        Self::new()
    }
}

impl DhcpServer {
    /// An empty lease table.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            leases: [Lease::FREE; POOL_SIZE],
        }
    }

    /// Forget every lease (the access point went down).
    pub fn reset(&mut self) {
        self.leases = [Lease::FREE; POOL_SIZE];
    }

    /// How many addresses are handed out or offered right now.
    #[must_use]
    pub fn live_leases(&self, now: u32) -> usize {
        self.leases.iter().filter(|l| l.live(now)).count()
    }

    /// The address this MAC holds, if any.
    #[must_use]
    pub fn lease_of(&self, mac: &[u8; MAC_LEN], now: u32) -> Option<[u8; 4]> {
        self.leases
            .iter()
            .position(|l| l.live(now) && &l.mac == mac)
            .map(address_of)
    }

    /// Handle one datagram received on port [`SERVER_PORT`].
    ///
    /// `now` is a seconds counter; only differences matter. `out` must be at
    /// least [`MAX_REPLY`] bytes. Returns `None` when there is nothing to
    /// send: a malformed or uninteresting packet, a RELEASE, or a new client
    /// while all [`POOL_SIZE`] addresses are live. Never panics.
    pub fn handle(&mut self, req: &[u8], now: u32, out: &mut [u8]) -> Option<Reply> {
        if req.len() < OPTIONS
            || out.len() < MAX_REPLY
            || req[OP] != BOOTREQUEST
            || req[HTYPE] != HTYPE_ETHER
            || req[HLEN] as usize != MAC_LEN
            || req[COOKIE..COOKIE + 4] != MAGIC
        {
            return None;
        }
        // No relay agents on the setup access point.
        if req[GIADDR..GIADDR + 4] != [0; 4] {
            return None;
        }

        let opts = Options::parse(&req[OPTIONS..])?;
        // A client that picked another server, or answered one, is not ours.
        if let Some(id) = opts.server_id {
            if id != SERVER_IP && opts.msg_type != Some(DISCOVER) {
                return None;
            }
        }

        let mut mac = [0u8; MAC_LEN];
        mac.copy_from_slice(&req[CHADDR..CHADDR + MAC_LEN]);

        match opts.msg_type? {
            DISCOVER => {
                let slot = self.slot_for(&mac, now)?;
                let ip = address_of(slot);
                self.leases[slot] = Lease {
                    mac,
                    expires: now.saturating_add(OFFER_SECS),
                    state: LeaseState::Offered,
                };
                let len = write_reply(req, OFFER, ip, out)?;
                Some(Reply {
                    len,
                    to: destination(req, ip, mac, false),
                })
            }
            REQUEST => {
                let mut wanted = opts.requested_ip;
                if wanted.is_none() {
                    let ciaddr = ip_at(req, CIADDR);
                    if ciaddr != [0; 4] {
                        wanted = Some(ciaddr);
                    }
                }
                // Without an address to confirm the request is malformed.
                let wanted = wanted?;
                match slot_of_address(wanted) {
                    // Ours, and either free or already this client's: bind it.
                    Some(slot) if !self.leases[slot].live(now) || self.leases[slot].mac == mac => {
                        self.leases[slot] = Lease {
                            mac,
                            expires: now.saturating_add(LEASE_SECS),
                            state: LeaseState::Bound,
                        };
                        let len = write_reply(req, ACK, wanted, out)?;
                        Some(Reply {
                            len,
                            to: destination(req, wanted, mac, false),
                        })
                    }
                    // Not from our pool, or held by somebody else.
                    _ => {
                        let len = write_reply(req, NAK, [0; 4], out)?;
                        Some(Reply {
                            len,
                            to: destination(req, [0; 4], mac, true),
                        })
                    }
                }
            }
            RELEASE => {
                let ciaddr = ip_at(req, CIADDR);
                for (i, lease) in self.leases.iter_mut().enumerate() {
                    let same_address = ciaddr == [0; 4] || ciaddr == address_of(i);
                    if lease.state != LeaseState::Free && lease.mac == mac && same_address {
                        *lease = Lease::FREE;
                    }
                }
                None
            }
            _ => None,
        }
    }

    /// The slot this MAC should get: the one it already has, else a free one,
    /// else an expired one. `None` while all [`POOL_SIZE`] are live.
    fn slot_for(&self, mac: &[u8; MAC_LEN], now: u32) -> Option<usize> {
        if let Some(i) = self
            .leases
            .iter()
            .position(|l| l.state != LeaseState::Free && &l.mac == mac)
        {
            return Some(i);
        }
        if let Some(i) = self.leases.iter().position(|l| l.state == LeaseState::Free) {
            return Some(i);
        }
        self.leases.iter().position(|l| !l.live(now))
    }
}

fn address_of(slot: usize) -> [u8; 4] {
    let mut ip = POOL_START;
    ip[3] = POOL_START[3] + slot as u8;
    ip
}

fn slot_of_address(ip: [u8; 4]) -> Option<usize> {
    if ip[..3] != POOL_START[..3] || ip[3] < POOL_START[3] {
        return None;
    }
    let slot = usize::from(ip[3] - POOL_START[3]);
    (slot < POOL_SIZE).then_some(slot)
}

fn ip_at(packet: &[u8], at: usize) -> [u8; 4] {
    let mut ip = [0u8; 4];
    ip.copy_from_slice(&packet[at..at + 4]);
    ip
}

/// RFC 2131 4.1: a client with an address gets a unicast, a client that asked
/// for broadcasts (or gets a NAK) gets a broadcast, anything else a unicast
/// that needs an ARP entry.
fn destination(req: &[u8], ip: [u8; 4], mac: [u8; MAC_LEN], is_nak: bool) -> Destination {
    let flags = u16::from_be_bytes([req[FLAGS], req[FLAGS + 1]]);
    let ciaddr = ip_at(req, CIADDR);
    if is_nak {
        return Destination::Broadcast;
    }
    if ciaddr != [0; 4] {
        return Destination::Client { ip: ciaddr };
    }
    if flags & FLAG_BROADCAST != 0 {
        return Destination::Broadcast;
    }
    Destination::ClientNoArp { ip, mac }
}

/// The three options we read. Everything else (55 parameter request list, 57
/// maximum message size, 12 host name, 61 client identifier, ...) is ignored.
#[derive(Default)]
struct Options {
    msg_type: Option<u8>,
    requested_ip: Option<[u8; 4]>,
    server_id: Option<[u8; 4]>,
}

impl Options {
    /// `None` for a truncated or otherwise malformed option field.
    fn parse(data: &[u8]) -> Option<Self> {
        let mut out = Self::default();
        let mut i = 0;
        while i < data.len() {
            let code = data[i];
            if code == OPT_PAD {
                i += 1;
                continue;
            }
            if code == OPT_END {
                break;
            }
            let len = usize::from(*data.get(i + 1)?);
            let value = data.get(i + 2..i + 2 + len)?;
            match code {
                OPT_MSG_TYPE if len == 1 => out.msg_type = Some(value[0]),
                OPT_REQUESTED_IP if len == 4 => {
                    let mut ip = [0u8; 4];
                    ip.copy_from_slice(value);
                    out.requested_ip = Some(ip);
                }
                OPT_SERVER_ID if len == 4 => {
                    let mut ip = [0u8; 4];
                    ip.copy_from_slice(value);
                    out.server_id = Some(ip);
                }
                _ => {}
            }
            i += 2 + len;
        }
        Some(out)
    }
}

/// Build an OFFER, ACK or NAK into `out`. Always writes [`MAX_REPLY`] bytes.
fn write_reply(req: &[u8], msg_type: u8, yiaddr: [u8; 4], out: &mut [u8]) -> Option<usize> {
    let out = out.get_mut(..MAX_REPLY)?;
    out.fill(0);
    out[OP] = BOOTREPLY;
    out[HTYPE] = HTYPE_ETHER;
    out[HLEN] = MAC_LEN as u8;
    out[XID..XID + 4].copy_from_slice(&req[XID..XID + 4]);
    out[FLAGS..FLAGS + 2].copy_from_slice(&req[FLAGS..FLAGS + 2]);
    if msg_type == ACK {
        out[CIADDR..CIADDR + 4].copy_from_slice(&req[CIADDR..CIADDR + 4]);
    }
    out[YIADDR..YIADDR + 4].copy_from_slice(&yiaddr);
    out[CHADDR..CHADDR + 16].copy_from_slice(&req[CHADDR..CHADDR + 16]);
    out[COOKIE..COOKIE + 4].copy_from_slice(&MAGIC);

    let mut i = OPTIONS;
    let mut put = |code: u8, value: &[u8], i: &mut usize| {
        out[*i] = code;
        out[*i + 1] = value.len() as u8;
        out[*i + 2..*i + 2 + value.len()].copy_from_slice(value);
        *i += 2 + value.len();
    };
    put(OPT_MSG_TYPE, &[msg_type], &mut i);
    put(OPT_SERVER_ID, &SERVER_IP, &mut i);
    if msg_type != NAK {
        put(OPT_LEASE_TIME, &LEASE_SECS.to_be_bytes(), &mut i);
        put(OPT_SUBNET, &SUBNET_MASK, &mut i);
        put(OPT_ROUTER, &SERVER_IP, &mut i);
        put(OPT_DNS, &SERVER_IP, &mut i);
    }
    out[i] = OPT_END;
    Some(MAX_REPLY)
}

#[cfg(test)]
mod tests {
    use super::*;

    const PHONE: [u8; 6] = [0x02, 0x11, 0x22, 0x33, 0x44, 0x55];

    /// A DISCOVER shaped like the ones a phone sends: broadcast flag set, a
    /// parameter request list (55), a maximum message size (57) and a host
    /// name (12) we have no use for.
    fn discover(mac: [u8; 6]) -> [u8; 320] {
        let mut p = request(mac, DISCOVER);
        let mut i = OPTIONS + 3;
        // 55 parameter request list
        p[i] = 55;
        p[i + 1] = 5;
        p[i + 2..i + 7].copy_from_slice(&[1, 3, 6, 15, 51]);
        i += 7;
        // 57 maximum DHCP message size
        p[i] = 57;
        p[i + 1] = 2;
        p[i + 2..i + 4].copy_from_slice(&1500u16.to_be_bytes());
        i += 4;
        // 12 host name
        p[i] = 12;
        p[i + 1] = 6;
        p[i + 2..i + 8].copy_from_slice(b"iphone");
        i += 8;
        // 61 client identifier
        p[i] = 61;
        p[i + 1] = 7;
        p[i + 2] = 1;
        p[i + 3..i + 9].copy_from_slice(&mac);
        i += 9;
        p[i] = OPT_END;
        p
    }

    fn request(mac: [u8; 6], msg_type: u8) -> [u8; 320] {
        let mut p = [0u8; 320];
        p[OP] = BOOTREQUEST;
        p[HTYPE] = HTYPE_ETHER;
        p[HLEN] = 6;
        p[XID..XID + 4].copy_from_slice(&0xdead_beefu32.to_be_bytes());
        p[FLAGS..FLAGS + 2].copy_from_slice(&FLAG_BROADCAST.to_be_bytes());
        p[CHADDR..CHADDR + 6].copy_from_slice(&mac);
        p[COOKIE..COOKIE + 4].copy_from_slice(&MAGIC);
        p[OPTIONS] = OPT_MSG_TYPE;
        p[OPTIONS + 1] = 1;
        p[OPTIONS + 2] = msg_type;
        p[OPTIONS + 3] = OPT_END;
        p
    }

    /// REQUEST with option 50 and, optionally, option 54.
    fn request_for(mac: [u8; 6], ip: [u8; 4], server_id: Option<[u8; 4]>) -> [u8; 320] {
        let mut p = request(mac, REQUEST);
        let mut i = OPTIONS + 3;
        p[i] = OPT_REQUESTED_IP;
        p[i + 1] = 4;
        p[i + 2..i + 6].copy_from_slice(&ip);
        i += 6;
        if let Some(id) = server_id {
            p[i] = OPT_SERVER_ID;
            p[i + 1] = 4;
            p[i + 2..i + 6].copy_from_slice(&id);
            i += 6;
        }
        p[i] = OPT_END;
        p
    }

    fn opt(reply: &[u8], code: u8) -> Option<&[u8]> {
        let mut i = OPTIONS;
        while i < reply.len() {
            match reply[i] {
                OPT_PAD => i += 1,
                OPT_END => return None,
                c => {
                    let len = usize::from(reply[i + 1]);
                    if c == code {
                        return Some(&reply[i + 2..i + 2 + len]);
                    }
                    i += 2 + len;
                }
            }
        }
        None
    }

    #[test]
    fn discover_gets_an_offer_with_every_option() {
        let mut s = DhcpServer::new();
        let mut out = [0u8; MAX_REPLY];
        let r = s.handle(&discover(PHONE), 100, &mut out).unwrap();
        assert_eq!(r.len, MAX_REPLY);
        // The broadcast flag was set, so the offer goes to the broadcast.
        assert_eq!(r.to, Destination::Broadcast);
        assert_eq!(out[OP], BOOTREPLY);
        assert_eq!(out[XID..XID + 4], 0xdead_beefu32.to_be_bytes());
        assert_eq!(out[YIADDR..YIADDR + 4], [192, 168, 4, 10]);
        assert_eq!(out[CHADDR..CHADDR + 6], PHONE);
        assert_eq!(out[COOKIE..COOKIE + 4], MAGIC);
        assert_eq!(opt(&out, OPT_MSG_TYPE), Some(&[OFFER][..]));
        assert_eq!(opt(&out, OPT_SERVER_ID), Some(&SERVER_IP[..]));
        assert_eq!(opt(&out, OPT_LEASE_TIME), Some(&3600u32.to_be_bytes()[..]));
        assert_eq!(opt(&out, OPT_SUBNET), Some(&[255, 255, 255, 0][..]));
        assert_eq!(opt(&out, OPT_ROUTER), Some(&SERVER_IP[..]));
        assert_eq!(opt(&out, OPT_DNS), Some(&SERVER_IP[..]));
    }

    #[test]
    fn request_after_offer_is_acked_and_bound() {
        let mut s = DhcpServer::new();
        let mut out = [0u8; MAX_REPLY];
        s.handle(&discover(PHONE), 10, &mut out).unwrap();
        let p = request_for(PHONE, [192, 168, 4, 10], Some(SERVER_IP));
        let r = s.handle(&p, 11, &mut out).unwrap();
        assert_eq!(opt(&out, OPT_MSG_TYPE), Some(&[ACK][..]));
        assert_eq!(out[YIADDR..YIADDR + 4], [192, 168, 4, 10]);
        assert_eq!(r.to, Destination::Broadcast);
        assert_eq!(s.lease_of(&PHONE, 11), Some([192, 168, 4, 10]));
        // One hour later the lease is gone.
        assert_eq!(s.lease_of(&PHONE, 11 + 3600), None);
    }

    #[test]
    fn same_mac_keeps_its_address() {
        let mut s = DhcpServer::new();
        let mut out = [0u8; MAX_REPLY];
        s.handle(&discover(PHONE), 0, &mut out).unwrap();
        s.handle(&request_for(PHONE, [192, 168, 4, 10], None), 0, &mut out)
            .unwrap();
        // Another client gets the next address.
        s.handle(&discover([2, 0, 0, 0, 0, 9]), 0, &mut out)
            .unwrap();
        assert_eq!(out[YIADDR..YIADDR + 4], [192, 168, 4, 11]);
        assert_eq!(s.live_leases(0), 2);
        // The phone comes back an hour later and discovers again: same
        // address. The other client never requested, so its offer is gone.
        s.handle(&discover(PHONE), 60, &mut out).unwrap();
        assert_eq!(out[YIADDR..YIADDR + 4], [192, 168, 4, 10]);
        assert_eq!(s.live_leases(60), 1);
    }

    #[test]
    fn unicast_when_the_client_has_an_address() {
        let mut s = DhcpServer::new();
        let mut out = [0u8; MAX_REPLY];
        // Renewing client: ciaddr set, broadcast flag clear.
        let mut p = request_for(PHONE, [192, 168, 4, 10], Some(SERVER_IP));
        p[FLAGS..FLAGS + 2].copy_from_slice(&0u16.to_be_bytes());
        p[CIADDR..CIADDR + 4].copy_from_slice(&[192, 168, 4, 10]);
        let r = s.handle(&p, 5, &mut out).unwrap();
        assert_eq!(
            r.to,
            Destination::Client {
                ip: [192, 168, 4, 10]
            }
        );
        // The ACK echoes ciaddr.
        assert_eq!(out[CIADDR..CIADDR + 4], [192, 168, 4, 10]);
    }

    #[test]
    fn no_broadcast_flag_and_no_ciaddr_needs_an_arp_entry() {
        let mut s = DhcpServer::new();
        let mut out = [0u8; MAX_REPLY];
        let mut p = discover(PHONE);
        p[FLAGS..FLAGS + 2].copy_from_slice(&0u16.to_be_bytes());
        let r = s.handle(&p, 0, &mut out).unwrap();
        assert_eq!(
            r.to,
            Destination::ClientNoArp {
                ip: [192, 168, 4, 10],
                mac: PHONE
            }
        );
    }

    #[test]
    fn request_for_a_foreign_address_is_naked() {
        let mut s = DhcpServer::new();
        let mut out = [0u8; MAX_REPLY];
        let r = s
            .handle(&request_for(PHONE, [10, 0, 0, 5], None), 0, &mut out)
            .unwrap();
        assert_eq!(opt(&out, OPT_MSG_TYPE), Some(&[NAK][..]));
        assert_eq!(out[YIADDR..YIADDR + 4], [0, 0, 0, 0]);
        assert_eq!(opt(&out, OPT_LEASE_TIME), None);
        assert_eq!(r.to, Destination::Broadcast);
        assert_eq!(s.live_leases(0), 0);
    }

    #[test]
    fn request_for_someone_elses_lease_is_naked() {
        let mut s = DhcpServer::new();
        let mut out = [0u8; MAX_REPLY];
        s.handle(&discover(PHONE), 0, &mut out).unwrap();
        s.handle(&request_for(PHONE, [192, 168, 4, 10], None), 0, &mut out)
            .unwrap();
        let other = [2, 0, 0, 0, 0, 7];
        s.handle(&request_for(other, [192, 168, 4, 10], None), 1, &mut out)
            .unwrap();
        assert_eq!(opt(&out, OPT_MSG_TYPE), Some(&[NAK][..]));
        assert_eq!(s.lease_of(&PHONE, 1), Some([192, 168, 4, 10]));
    }

    #[test]
    fn request_without_an_address_is_ignored() {
        let mut s = DhcpServer::new();
        let mut out = [0u8; MAX_REPLY];
        assert!(s.handle(&request(PHONE, REQUEST), 0, &mut out).is_none());
    }

    #[test]
    fn a_selected_other_server_is_left_alone() {
        let mut s = DhcpServer::new();
        let mut out = [0u8; MAX_REPLY];
        let p = request_for(PHONE, [192, 168, 4, 10], Some([192, 168, 4, 99]));
        assert!(s.handle(&p, 0, &mut out).is_none());
        assert_eq!(s.live_leases(0), 0);
    }

    #[test]
    fn release_frees_the_lease() {
        let mut s = DhcpServer::new();
        let mut out = [0u8; MAX_REPLY];
        s.handle(&discover(PHONE), 0, &mut out).unwrap();
        s.handle(&request_for(PHONE, [192, 168, 4, 10], None), 0, &mut out)
            .unwrap();
        let mut p = request(PHONE, RELEASE);
        p[CIADDR..CIADDR + 4].copy_from_slice(&[192, 168, 4, 10]);
        assert!(s.handle(&p, 1, &mut out).is_none());
        assert_eq!(s.live_leases(1), 0);
        assert_eq!(s.lease_of(&PHONE, 1), None);
    }

    #[test]
    fn a_full_table_answers_nothing_and_expiry_frees_it() {
        let mut s = DhcpServer::new();
        let mut out = [0u8; MAX_REPLY];
        for i in 0..POOL_SIZE {
            let mac = [2, 0, 0, 0, 0, i as u8];
            s.handle(&discover(mac), 0, &mut out).unwrap();
            s.handle(&request_for(mac, address_of(i), None), 0, &mut out)
                .unwrap();
        }
        assert_eq!(s.live_leases(0), POOL_SIZE);
        // A ninth client while all eight are live: no answer at all.
        assert!(s.handle(&discover(PHONE), 10, &mut out).is_none());
        // After the hour, the oldest expired lease is reused.
        let r = s.handle(&discover(PHONE), 3601, &mut out).unwrap();
        assert_eq!(r.len, MAX_REPLY);
        assert_eq!(out[YIADDR..YIADDR + 4], [192, 168, 4, 10]);
        assert_eq!(s.live_leases(3601), 1);
    }

    #[test]
    fn an_unanswered_offer_expires_quickly() {
        let mut s = DhcpServer::new();
        let mut out = [0u8; MAX_REPLY];
        s.handle(&discover(PHONE), 0, &mut out).unwrap();
        assert_eq!(s.live_leases(0), 1);
        assert_eq!(s.live_leases(31), 0);
    }

    #[test]
    fn uninteresting_and_malformed_packets_are_dropped() {
        let mut s = DhcpServer::new();
        let mut out = [0u8; MAX_REPLY];
        // A reply, not a request.
        let mut p = discover(PHONE);
        p[OP] = BOOTREPLY;
        assert!(s.handle(&p, 0, &mut out).is_none());
        // Token ring hardware type.
        let mut p = discover(PHONE);
        p[HTYPE] = 6;
        assert!(s.handle(&p, 0, &mut out).is_none());
        // Wrong hardware length.
        let mut p = discover(PHONE);
        p[HLEN] = 16;
        assert!(s.handle(&p, 0, &mut out).is_none());
        // Bad magic cookie.
        let mut p = discover(PHONE);
        p[COOKIE] = 0;
        assert!(s.handle(&p, 0, &mut out).is_none());
        // Relayed.
        let mut p = discover(PHONE);
        p[GIADDR] = 10;
        assert!(s.handle(&p, 0, &mut out).is_none());
        // No message type option.
        let mut p = discover(PHONE);
        p[OPTIONS] = OPT_END;
        assert!(s.handle(&p, 0, &mut out).is_none());
        // Option length running off the end.
        let mut p = [0u8; OPTIONS + 3];
        p[..OPTIONS].copy_from_slice(&discover(PHONE)[..OPTIONS]);
        p[OPTIONS] = OPT_MSG_TYPE;
        p[OPTIONS + 1] = 40;
        assert!(s.handle(&p, 0, &mut out).is_none());
        // DECLINE and INFORM are ignored.
        assert!(s.handle(&request(PHONE, 4), 0, &mut out).is_none());
        assert!(s.handle(&request(PHONE, 8), 0, &mut out).is_none());
        // Too small an output buffer: nothing written, nothing sent.
        let mut small = [0u8; 100];
        assert!(s.handle(&discover(PHONE), 0, &mut small).is_none());
        assert_eq!(s.live_leases(0), 0);
    }

    #[test]
    fn every_prefix_of_a_valid_packet_is_survivable() {
        let full = discover(PHONE);
        let mut out = [0u8; MAX_REPLY];
        for n in 0..=full.len() {
            let mut s = DhcpServer::new();
            let _ = s.handle(&full[..n], 7, &mut out);
        }
        let full = request_for(PHONE, [192, 168, 4, 12], Some(SERVER_IP));
        for n in 0..=full.len() {
            let mut s = DhcpServer::new();
            let _ = s.handle(&full[..n], 7, &mut out);
        }
        // Every single-byte mutation of the option field, too.
        for i in OPTIONS..full.len() {
            for b in [0u8, 1, 50, 53, 54, 255] {
                let mut p = full;
                p[i] = b;
                let mut s = DhcpServer::new();
                let _ = s.handle(&p, 7, &mut out);
            }
        }
    }

    #[test]
    fn state_is_small() {
        // 8 leases of 12 bytes: the whole DHCP server fits in a static.
        assert_eq!(core::mem::size_of::<DhcpServer>(), 96);
    }
}
