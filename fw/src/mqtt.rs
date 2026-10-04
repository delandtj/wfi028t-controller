//! MQTT 5 client and Home Assistant discovery: the ADR's component 3.
//!
//! One task, one TCP socket, one retained JSON document. It subscribes to the
//! command topics, validates every payload through [`entity::parse_set`] and
//! `hp_model`, and hands the result to the bus master through
//! [`master::submit`]. It never touches the bus itself and never blocks the
//! bus task: polling continues whatever the broker, the WiFi or Home
//! Assistant are doing (ADR, "Fails safe").
//!
//! | Topic | Direction | Payload |
//! |---|---|---|
//! | `wfi028t/availability` | out, retained, also the last will | `online` / `offline` |
//! | `wfi028t/state` | out, retained | one flat JSON object, [`json`] |
//! | `wfi028t/<entity>/set` | in | one value, [`entity::parse_set`] |
//! | `homeassistant/<component>/wfi028t/<object_id>/config` | out, retained | discovery, [`entity::write_discovery`] |
//! | `homeassistant/status` | in | `online` triggers a discovery republish |
//!
//! State goes out when it changes - the new document is compared against the
//! last one published, minus the monotonic counters at its end (see
//! [`json::volatile_free`]), so a quiet heat pump produces no traffic at all -
//! and in full every [`REFRESH`].
//!
//! # MQTT 5, through rust-mqtt 0.6
//!
//! The broker is Mosquitto (the Home Assistant add-on), which speaks MQTT 5,
//! so 3.1.1 was a constraint nothing imposed. `rust-mqtt` 0.6 is MQTT 5 only
//! and is built on `embedded-io-async` 0.7 and `heapless` 0.9, which is
//! exactly what embassy-net 0.9's sockets want: it drops in without a shim
//! and it means 500 lines of packet encoding we no longer own.
//!
//! Three choices are worth naming:
//!
//! - **`alloc`, not `bump`.** The client needs somewhere to put the
//!   variable-length fields of a received packet. `BumpBuffer` avoids the
//!   heap but hands back slices borrowed from one backing array, which must
//!   be invalidated with an `unsafe fn reset()` whose soundness condition is
//!   "no value from the last packet is still alive" - an invariant this event
//!   loop would have to re-prove on every edit. `AllocBuffer` gives owned
//!   `Box<[u8]>` payloads instead, so a received message borrows nothing and
//!   the event loop can publish while holding it. `esp-alloc` is already set
//!   up (`main.rs`, `HEAP_SIZE`) because `esp-radio` allocates, and
//!   [`MAX_PACKET`] bounds what one packet can ask for.
//! - **A client per connection.** `Client::connect` may only be called on a
//!   freshly built client, or after a clean `disconnect`, or after `abort`
//!   following an unrecoverable error. Building one per attempt makes that
//!   trivially true and gives every reconnect a clean session state; the
//!   client is a few hundred bytes with the queue sizes below.
//! - **[`MAX_PACKET`] in CONNECT.** Without it the client would accept (and
//!   try to allocate) a packet of up to 268 MB. With it, the broker must not
//!   send anything larger, and the client refuses it if it does. The
//!   difference from the hand-rolled client is that an oversized message is
//!   now dropped broker-side rather than read and discarded here, so it does
//!   not show up in the capture stream.
//!
//! # Where the pieces live
//!
//! | Module | Job |
//! |---|---|
//! | [`entity`] | the entity table, discovery payloads, payload parsing, pure |
//! | [`json`] | the state document, pure |
//! | [`config`] | broker address and credentials, flash record, `mqtt` command, pure |
//! | this file | the socket, the reconnect loop and the event loop |

use core::cell::Cell;
use core::fmt::Write as _;
use core::num::NonZero;
use core::sync::atomic::{AtomicU32, AtomicU8, Ordering};

use embassy_executor::Spawner;
use embassy_futures::select::{select, select4, Either, Either4};
use embassy_net::tcp::TcpSocket;
use embassy_net::{IpEndpoint, Ipv4Address, Stack};
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::blocking_mutex::Mutex as BlockingMutex;
use embassy_sync::signal::Signal;
use embassy_time::{with_timeout, Duration, Instant, Timer};
use heapless::String;
use static_cell::StaticCell;

use rust_mqtt::buffer::AllocBuffer;
use rust_mqtt::client::event::{Event, Publish};
use rust_mqtt::client::options::{
    ConnectOptions, DisconnectOptions, PublicationOptions, RetainHandling, SubscriptionOptions,
    TopicReference, WillOptions,
};
use rust_mqtt::client::{Client, MqttError};
use rust_mqtt::config::KeepAlive;
use rust_mqtt::types::{MqttBinary, MqttString, TopicFilter, TopicName};
use rust_mqtt::Bytes;

use modbus_sniffer_core::Line;

use crate::master::{self, CommandReport, LinkState, OpMode, Snapshot};

pub mod config;
pub mod entity;
pub mod json;

pub use config::{Broker, Request, Stored};

/// Keepalive promised to the broker in CONNECT.
const KEEPALIVE: KeepAlive = match NonZero::new(60u16) {
    Some(seconds) => KeepAlive::Seconds(seconds),
    None => KeepAlive::Infinite,
};

/// How often a PINGREQ goes out while the connection is otherwise idle. Well
/// inside [`KEEPALIVE`], so a slow broker cannot make us look dead.
const PING_INTERVAL: Duration = Duration::from_secs(20);

/// Full state refresh, regardless of change (the ADR's "plus a full refresh
/// every 60 s").
const REFRESH: Duration = Duration::from_secs(60);

/// How long the TCP connect attempt gets.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// How long the broker gets to answer CONNECT with a CONNACK.
const CONNACK_TIMEOUT: Duration = Duration::from_secs(10);

/// How long the TCP stack puts up with an unacknowledged connection before it
/// aborts the socket. Generous against [`PING_INTERVAL`], which is what keeps
/// an idle connection acknowledged.
const SOCKET_TIMEOUT: Duration = Duration::from_secs(90);

/// Reconnect backoff: doubles on every failure up to the maximum.
const BACKOFF_MIN: Duration = Duration::from_secs(2);
const BACKOFF_MAX: Duration = Duration::from_secs(60);

/// Pause between two discovery publishes, so a 36-entity burst stays polite
/// to the broker and keeps handing the executor back to the bus task.
const DISCOVERY_PACE: Duration = Duration::from_millis(10);

/// Socket buffers. Both only bound how much can be in flight at once, not
/// how big a packet may be: the client reads and writes incrementally and
/// handles short reads and writes itself. TX is sized to swallow the largest
/// discovery packet ([`DISCOVERY_MAX`] plus its topic) without a round trip.
const SOCKET_RX: usize = 512;
const SOCKET_TX: usize = 1024;

/// Largest packet the broker may send us, announced in CONNECT.
///
/// Everything we subscribe to is a handful of bytes, and the CONNACK of a
/// sane broker is well under a hundred. This is the ceiling on what one
/// received packet can allocate, which is the reason it is set at all.
const MAX_PACKET: NonZero<u32> = match NonZero::new(1024) {
    Some(limit) => limit,
    None => unreachable!(),
};

/// Buffer for the state document. The host test
/// `json::tests::the_document_fits_the_task_buffer` holds this size.
const STATE_MAX: usize = 1024;

/// Buffer for one discovery payload. The host test
/// `entity::tests::every_discovery_payload_is_balanced_json_with_the_device_block`
/// holds this size; the largest payload today is 528 bytes (`stop_at_target`),
/// so there is room for a fifth again as much. A payload that does not fit is
/// named in the capture stream and skipped, never published truncated.
const DISCOVERY_MAX: usize = 640;

/// Buffer for a topic. The longest today is the 61-byte discovery topic of
/// `stop_at_target`; a topic that does not fit is named and skipped.
const TOPIC_MAX: usize = 96;

/// How much of the last command report the diagnostic sensor carries,
/// sequence number included.
const REPORT_MAX: usize = 96;

/// Characters of a bad payload echoed into the capture stream.
const CLIP: usize = 24;

// --- Client queue sizes, as const generics of `Client` -----------------------

/// SUBSCRIBE packets in flight: the command filter and `homeassistant/status`
/// go out back to back, without waiting for the first SUBACK.
const SUBSCRIBE_MAXIMUM: usize = 2;

/// Incoming QoS 1/2 publications. We subscribe at QoS 0, so the broker never
/// sends one; the client requires at least 1.
const RECEIVE_MAXIMUM: usize = 1;

/// Outgoing QoS 1/2 publications. Everything here is QoS 0.
const SEND_MAXIMUM: usize = 0;

/// Subscription identifiers per received PUBLISH. We never ask for any.
const MAX_SUBSCRIPTION_IDENTIFIERS: usize = 0;

/// User properties per packet. One, not zero, so the client can spot a broker
/// that sends properties we told it not to.
const MAX_USER_PROPERTIES: usize = 1;

/// Topic aliases the broker may use towards us. None: a received PUBLISH
/// always carries its topic name, which is what [`on_message`] routes on.
const MAX_INCOMING_TOPIC_ALIASES: usize = 0;

/// Topic aliases we use towards the broker. None: discovery touches 36
/// distinct topics once per connection, so an alias table buys nothing.
const MAX_OUTGOING_TOPIC_ALIASES: usize = 0;

/// The client, with every queue sized for what this firmware actually does.
type Mqtt<'socket, 'buffer> = Client<
    'static,
    'buffer,
    TcpSocket<'socket>,
    AllocBuffer,
    SUBSCRIBE_MAXIMUM,
    RECEIVE_MAXIMUM,
    SEND_MAXIMUM,
    MAX_SUBSCRIPTION_IDENTIFIERS,
    MAX_USER_PROPERTIES,
    MAX_INCOMING_TOPIC_ALIASES,
    MAX_OUTGOING_TOPIC_ALIASES,
>;

/// One received application message.
type Message<'a> = Publish<'a, MAX_SUBSCRIPTION_IDENTIFIERS, MAX_USER_PROPERTIES>;

// --- Topics known at build time ---------------------------------------------

/// Whether a literal fits MQTT's string field: non-empty, inside the 16-bit
/// length prefix, no NUL.
const fn is_mqtt_string(text: &str) -> bool {
    let bytes = text.as_bytes();
    if bytes.is_empty() || bytes.len() > u16::MAX as usize {
        return false;
    }
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == 0 {
            return false;
        }
        i += 1;
    }
    true
}

/// An MQTT string from a literal, checked while the image is built.
///
/// The checked constructors cannot be used here: with `alloc` on, `MqttString`
/// owns a `Box` in one of its variants, so a `Result<MqttString, _>` is not
/// droppable in a `const fn`.
const fn mqtt_str(text: &str) -> MqttString<'_> {
    assert!(is_mqtt_string(text), "not a legal MQTT string");
    MqttString::from_str_unchecked(text)
}

/// A topic name from a literal.
///
/// Wildcards are rejected here; the rest of the topic-name rules are
/// `debug_assert`ed by `TopicName::new_unchecked` and held by
/// `entity::tests::topics_follow_the_documented_layout`.
const fn topic(text: &str) -> TopicName<'_> {
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        assert!(
            bytes[i] != b'+' && bytes[i] != b'#',
            "a topic name cannot contain a wildcard"
        );
        i += 1;
    }
    TopicName::new_unchecked(mqtt_str(text))
}

/// Binary payload from a literal, checked while the image is built.
const fn mqtt_bytes(text: &str) -> MqttBinary<'_> {
    assert!(
        text.len() <= u16::MAX as usize,
        "payload does not fit an MQTT binary field"
    );
    MqttBinary::from_slice_unchecked(text.as_bytes())
}

/// The MQTT client id, which is also the device id and the topic base.
const CLIENT_ID: MqttString<'static> = mqtt_str(entity::DEVICE_ID);

/// Availability topic, published on connect and as the last will.
const AVAILABILITY: TopicName<'static> = topic(entity::TOPIC_AVAILABILITY);

/// The one retained state document.
const STATE: TopicName<'static> = topic(entity::TOPIC_STATE);

/// Last will payload.
const OFFLINE: MqttBinary<'static> = mqtt_bytes(entity::PAYLOAD_OFFLINE);

static RX: StaticCell<[u8; SOCKET_RX]> = StaticCell::new();
static TX: StaticCell<[u8; SOCKET_TX]> = StaticCell::new();

/// The stored broker setting, as last read from or written to flash.
static STORED: BlockingMutex<CriticalSectionRawMutex, Cell<Stored>> =
    BlockingMutex::new(Cell::new(Stored::Unset));

/// Raised by the `mqtt` command: drop the connection and start over with the
/// new configuration.
static RECONFIGURED: Signal<CriticalSectionRawMutex, ()> = Signal::new();

/// [`Connection`] as a code, for the `status` line.
static CONNECTION: AtomicU8 = AtomicU8::new(0);

/// Messages published since boot.
static PUBLISHED: AtomicU32 = AtomicU32::new(0);
/// Command messages received since boot.
static RECEIVED: AtomicU32 = AtomicU32::new(0);
/// Command messages refused or dropped (bad payload, listen mode, full queue).
static DROPPED: AtomicU32 = AtomicU32::new(0);
/// Connection attempts that failed, or connections that broke.
static FAILURES: AtomicU32 = AtomicU32::new(0);

/// What the MQTT task is doing, for the `status` line and the capture stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Connection {
    /// No broker configured, at build time or in flash.
    Disabled,
    /// Waiting for WiFi and a DHCP lease.
    NoNetwork,
    /// TCP or MQTT handshake in progress.
    Connecting,
    /// Connected, subscribed, publishing.
    Online,
    /// Backing off after a failure.
    Retrying,
}

impl Connection {
    /// Short tag for the `status` line.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Disabled => "disabled",
            Self::NoNetwork => "no-network",
            Self::Connecting => "connecting",
            Self::Online => "online",
            Self::Retrying => "retrying",
        }
    }

    const fn code(self) -> u8 {
        match self {
            Self::Disabled => 0,
            Self::NoNetwork => 1,
            Self::Connecting => 2,
            Self::Online => 3,
            Self::Retrying => 4,
        }
    }

    const fn from_code(code: u8) -> Self {
        match code {
            1 => Self::NoNetwork,
            2 => Self::Connecting,
            3 => Self::Online,
            4 => Self::Retrying,
            _ => Self::Disabled,
        }
    }
}

// ---------------------------------------------------------------------------
// Configuration, shared with the line interface and the flash store
// ---------------------------------------------------------------------------

/// Install the setting read from flash. Called once from `main`, before the
/// task starts.
pub fn load(stored: Stored) {
    STORED.lock(|cell| cell.set(stored));
    set_connection(match config::effective(stored) {
        Some(_) => Connection::NoNetwork,
        None => Connection::Disabled,
    });
}

/// Install a new setting and make the task act on it at once.
pub fn configure(stored: Stored) {
    STORED.lock(|cell| cell.set(stored));
    RECONFIGURED.signal(());
}

/// The setting as stored (which is not the same as the one in force).
#[must_use]
pub fn stored() -> Stored {
    STORED.lock(Cell::get)
}

/// The broker in force: the stored one, or the build-time default.
#[must_use]
pub fn effective() -> Option<Broker> {
    config::effective(stored())
}

/// What the task is doing.
#[must_use]
pub fn connection() -> Connection {
    Connection::from_code(CONNECTION.load(Ordering::Relaxed))
}

/// Messages published since boot.
#[must_use]
pub fn published() -> u32 {
    PUBLISHED.load(Ordering::Relaxed)
}

/// Command messages received since boot.
#[must_use]
pub fn received() -> u32 {
    RECEIVED.load(Ordering::Relaxed)
}

/// Command messages that did not reach the bus master.
#[must_use]
pub fn dropped() -> u32 {
    DROPPED.load(Ordering::Relaxed)
}

/// Failed connection attempts and broken connections.
#[must_use]
pub fn failures() -> u32 {
    FAILURES.load(Ordering::Relaxed)
}

fn set_connection(state: Connection) {
    CONNECTION.store(state.code(), Ordering::Relaxed);
}

// ---------------------------------------------------------------------------
// The task
// ---------------------------------------------------------------------------

/// Start the MQTT task. Called from [`crate::net`], which owns the stack.
pub fn start(spawner: &Spawner, stack: Stack<'static>) {
    spawner.spawn(mqtt_task(stack).unwrap());
}

#[embassy_executor::task]
async fn mqtt_task(stack: Stack<'static>) {
    let rx = RX.init([0; SOCKET_RX]);
    let tx = TX.init([0; SOCKET_TX]);

    // Two of the six snapshot/outcome slots, taken once and held for good.
    // Nothing else has taken any at this point in the boot.
    let (Some(mut snapshots), Some(mut outcomes)) =
        (master::SNAPSHOT.receiver(), master::OUTCOMES.receiver())
    else {
        crate::publish_note(format_args!("mqtt disabled: no snapshot slot free"));
        return;
    };

    // A zero-sized handle on the global allocator, reborrowed for each client.
    let mut buffer = AllocBuffer;
    let mut session = Session::new();
    let mut backoff = BACKOFF_MIN;

    loop {
        // Cleared BEFORE the configuration is read, so a `mqtt` command that
        // lands between the two is not lost: the signal stays raised and the
        // wait below (or the one in the event loop) returns at once.
        RECONFIGURED.reset();

        let Some(broker) = effective() else {
            set_connection(Connection::Disabled);
            RECONFIGURED.wait().await;
            backoff = BACKOFF_MIN;
            continue;
        };

        set_connection(Connection::NoNetwork);
        stack.wait_config_up().await;

        set_connection(Connection::Connecting);
        let mut socket = TcpSocket::new(stack, &mut rx[..], &mut tx[..]);
        // The stack's own timeout matters as much as the MQTT keepalive: a
        // broker that stops acknowledging while the client is inside a write
        // would otherwise park this task for good, and the ping arm of the
        // event loop never gets to run. Probing every [`PING_INTERVAL`] keeps
        // a healthy idle connection well inside it.
        socket.set_timeout(Some(SOCKET_TIMEOUT));
        socket.set_keep_alive(Some(PING_INTERVAL));

        // Only a reconfiguration ends a session cleanly.
        let trouble = connect_and_serve(
            socket,
            &mut buffer,
            &broker,
            &mut session,
            &mut snapshots,
            &mut outcomes,
        )
        .await
        .err();

        match trouble {
            None => {
                crate::publish_note(format_args!("mqtt reconfigured, reconnecting"));
                backoff = BACKOFF_MIN;
                continue;
            }
            Some(trouble) => {
                FAILURES.fetch_add(1, Ordering::Relaxed);
                let [a, b, c, d] = broker.host();
                crate::publish_note(format_args!(
                    "mqtt {trouble} ({a}.{b}.{c}.{d}:{}), retry in {} s",
                    broker.port(),
                    backoff.as_secs()
                ));
            }
        }

        set_connection(Connection::Retrying);
        let _ = select(Timer::after(backoff), RECONFIGURED.wait()).await;
        backoff = (backoff * 2).min(BACKOFF_MAX);
    }
}

/// What ended a session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Trouble {
    /// The TCP connection could not be made.
    TcpFailed,
    /// The TCP connection or the MQTT handshake timed out.
    Timeout,
    /// The broker refused the CONNECT or sent a DISCONNECT (reason code).
    Refused(u8),
    /// The broker said something the client cannot accept.
    Protocol,
    /// A received packet could not be allocated.
    OutOfMemory,
    /// The configured credentials are not a legal MQTT user name or password.
    BadCredentials,
    /// The socket died.
    SocketClosed,
    /// The broker stopped answering PINGREQ.
    NoPingResponse,
}

impl Trouble {
    const fn as_str(self) -> &'static str {
        match self {
            Self::TcpFailed => "tcp connect failed",
            Self::Timeout => "handshake timed out",
            Self::Refused(_) => "broker refused or closed the connection",
            Self::Protocol => "protocol error",
            Self::OutOfMemory => "out of memory receiving a packet",
            Self::BadCredentials => "credentials are not a legal MQTT user name or password",
            Self::SocketClosed => "connection lost",
            Self::NoPingResponse => "broker stopped answering pings",
        }
    }
}

impl core::fmt::Display for Trouble {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(self.as_str())?;
        if let Self::Refused(code) = self {
            write!(f, ", reason 0x{code:02x}")?;
        }
        Ok(())
    }
}

impl<const N: usize> From<MqttError<'_, N>> for Trouble {
    fn from(error: MqttError<'_, N>) -> Self {
        match error {
            MqttError::Network(_) => Self::SocketClosed,
            MqttError::Alloc => Self::OutOfMemory,
            MqttError::Disconnect { reason, .. } => Self::Refused(reason.value()),
            _ => Self::Protocol,
        }
    }
}

/// Short tag for an [`MqttError`] the client refused to act on, for the
/// capture stream. Only the ones this firmware can actually provoke are
/// spelled out; the rest belong to QoS and authentication flows we do not use.
fn reason<const N: usize>(error: &MqttError<'_, N>) -> &'static str {
    match error {
        MqttError::Network(_) => "network error",
        MqttError::Server => "protocol error",
        MqttError::Alloc => "out of memory",
        MqttError::Disconnect { .. } => "broker disconnected",
        MqttError::UnsupportedByServer => "broker does not support it",
        MqttError::PacketMaximumLengthExceeded | MqttError::ServerMaximumPacketSizeExceeded => {
            "packet too large"
        }
        MqttError::SessionBuffer | MqttError::AllPacketIdentifiersUsed => "client queue full",
        _ => "refused by the client",
    }
}

/// Open the TCP connection, hand the socket to a fresh client, and serve
/// until something breaks. Dropping the client at the end of this function
/// drops the socket with it, which removes it from the stack - the abort the
/// caller used to do by hand.
async fn connect_and_serve(
    mut socket: TcpSocket<'_>,
    buffer: &mut AllocBuffer,
    broker: &Broker,
    session: &mut Session,
    snapshots: &mut master::SnapshotReceiver,
    outcomes: &mut master::OutcomeReceiver,
) -> Result<(), Trouble> {
    let endpoint = IpEndpoint::from((Ipv4Address::from(broker.host()), broker.port()));
    match with_timeout(CONNECT_TIMEOUT, socket.connect(endpoint)).await {
        Ok(Ok(())) => {}
        Ok(Err(_)) => return Err(Trouble::TcpFailed),
        Err(_) => return Err(Trouble::Timeout),
    }

    let mut client = Client::new(buffer);
    session_with(&mut client, socket, broker, session, snapshots, outcomes).await
}

/// Connect, announce, and serve until something breaks.
///
/// `Ok(())` means the configuration changed and the caller should start over;
/// every other ending is a [`Trouble`].
async fn session_with<'socket, 'buffer>(
    client: &mut Mqtt<'socket, 'buffer>,
    socket: TcpSocket<'socket>,
    broker: &Broker,
    session: &mut Session,
    snapshots: &mut master::SnapshotReceiver,
    outcomes: &mut master::OutcomeReceiver,
) -> Result<(), Trouble> {
    // CONNECT, with the availability topic as the last will so the broker
    // marks the device offline if this connection dies without a goodbye.
    // Clean start: the controller keeps no session state worth resuming, and
    // it is what makes a reconnect idempotent.
    let mut options = ConnectOptions::new()
        .clean_start()
        .keep_alive(KEEPALIVE)
        .maximum_packet_size(MAX_PACKET)
        .will(WillOptions::new(AVAILABILITY, OFFLINE).retain());
    if let Some(user) = broker.user_opt() {
        let Ok(user) = MqttString::from_str(user) else {
            return Err(Trouble::BadCredentials);
        };
        options = options.user_name(user);
    }
    if let Some(pass) = broker.pass_opt() {
        // Never logged, here or anywhere: `Broker`'s `Debug` prints "set".
        let Ok(pass) = MqttBinary::from_slice(pass.as_bytes()) else {
            return Err(Trouble::BadCredentials);
        };
        options = options.password(pass);
    }

    match with_timeout(
        CONNACK_TIMEOUT,
        client.connect(socket, &options, Some(CLIENT_ID)),
    )
    .await
    {
        Ok(Ok(_)) => {}
        Ok(Err(error)) => return Err(error.into()),
        Err(_) => return Err(Trouble::Timeout),
    }

    let [a, b, c, d] = broker.host();
    crate::publish_note(format_args!(
        "mqtt connected to {a}.{b}.{c}.{d}:{}, publishing {} entities",
        broker.port(),
        entity::ENTITIES.len()
    ));
    if !client.server_config().retain_supported {
        // Every topic here is retained, so this broker cannot host this
        // device. Say so once rather than once per publish.
        crate::publish_note(format_args!(
            "mqtt broker does not support retained messages: state and discovery will not stick"
        ));
    }

    publish(client, AVAILABILITY, entity::PAYLOAD_ONLINE, true).await?;
    publish_discovery(client).await?;

    // A retained command would be re-delivered on every single reconnect and
    // re-apply itself to the heat pump long after whoever published it meant
    // it, so the broker is told not to send the retained store for this
    // filter at all. `retain_as_published` keeps the flag the publisher set
    // on messages that do arrive, which is what lets [`on_message`] refuse a
    // stray `mosquitto_pub -r` while the connection is up: without it MQTT
    // would have the broker clear the flag on forwarded messages.
    subscribe(
        client,
        entity::TOPIC_COMMAND_FILTER,
        &SubscriptionOptions::new()
            .retain_as_published()
            .retain_handling(RetainHandling::NeverSend),
    )
    .await?;
    // Home Assistant's birth message, on the other hand, is worth having from
    // the retained store: it is how a device that connects after HA learns
    // that HA is up.
    subscribe(client, entity::TOPIC_HA_STATUS, &SubscriptionOptions::new()).await?;

    // First state document of this connection, unconditionally.
    session.published.clear();
    publish_state(client, session, snapshots.try_get().as_ref()).await?;

    let mut refresh_at = Instant::now() + REFRESH;
    let mut ping_at = Instant::now() + PING_INTERVAL;
    let mut awaiting_pong = false;

    set_connection(Connection::Online);

    loop {
        let deadline = refresh_at.min(ping_at);
        // `poll_header` is the one arm that touches the socket, and it is the
        // only read in this crate's API that is cancel-safe: losing the race
        // to a snapshot or a timer cannot lose bytes. Its body is then read
        // to completion right away, which is what `poll_body` requires.
        let event = select4(
            client.poll_header(),
            snapshots.changed(),
            outcomes.changed(),
            select(Timer::at(deadline), RECONFIGURED.wait()),
        )
        .await;

        match event {
            // A packet from the broker.
            Either4::First(header) => match client.poll_body(header?).await? {
                Event::Publish(message) => {
                    // The payload is owned (`AllocBuffer`), so handling it can
                    // publish without aliasing anything the client holds.
                    on_message(client, session, snapshots, &message).await?;
                }
                Event::Pingresp => awaiting_pong = false,
                Event::Suback(suback) if suback.reason_code.is_erroneous() => {
                    // Commands will simply never arrive; say which.
                    crate::publish_note(format_args!(
                        "mqtt broker refused a subscription, reason 0x{:02x}",
                        suback.reason_code.value()
                    ));
                }
                // Nothing else can reach us: QoS 0 both ways, no aliases, no
                // enhanced authentication.
                _ => {}
            },

            // New heat pump state: publish only if the document changed.
            Either4::Second(snapshot) => {
                publish_state(client, session, Some(&snapshot)).await?;
            }

            // A command outcome: into the diagnostic sensor, then publish.
            Either4::Third(report) => {
                session.set_report(&report);
                publish_state(client, session, snapshots.try_get().as_ref()).await?;
            }

            // The refresh/keepalive timer.
            Either4::Fourth(Either::First(())) => {
                let now = Instant::now();
                if now >= refresh_at {
                    refresh_at = now + REFRESH;
                    session.published.clear();
                    publish_state(client, session, snapshots.try_get().as_ref()).await?;
                }
                if now >= ping_at {
                    if awaiting_pong {
                        return Err(Trouble::NoPingResponse);
                    }
                    ping_at = now + PING_INTERVAL;
                    awaiting_pong = true;
                    client.ping().await?;
                }
            }
            // The `mqtt` command changed the configuration.
            Either4::Fourth(Either::Second(())) => {
                // Say goodbye properly: a retained "offline" and a DISCONNECT
                // with reason Success, so the broker does not fire the will
                // and HA sees one clean transition.
                let _ = publish(client, AVAILABILITY, entity::PAYLOAD_OFFLINE, true).await;
                if let Ok(mut socket) = client.disconnect(&DisconnectOptions::new()).await {
                    socket.abort();
                    let _ = socket.flush().await;
                }
                return Ok(());
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Publishing
// ---------------------------------------------------------------------------

/// A topic name from a runtime string, `None` if MQTT would not take it.
fn topic_of(text: &str) -> Option<TopicName<'_>> {
    TopicName::new(MqttString::from_str(text).ok()?)
}

/// Subscribe to one filter at QoS 0.
///
/// A broker that refuses the subscription outright - one without wildcard
/// support, say - is named in the capture stream rather than dropped, because
/// reconnecting would only ask it the same question again. The connection is
/// still worth having: state and discovery keep going out, only commands
/// never arrive. The SUBACK is logged by the event loop.
async fn subscribe(
    client: &mut Mqtt<'_, '_>,
    topic: &str,
    options: &SubscriptionOptions<'_>,
) -> Result<(), Trouble> {
    let Some(filter) = MqttString::from_str(topic).ok().and_then(TopicFilter::new) else {
        crate::publish_note(format_args!("mqtt {topic} is not a legal topic filter"));
        return Ok(());
    };
    match client.subscribe(filter, options).await {
        Ok(_) => Ok(()),
        Err(error) if error.is_recoverable() => {
            crate::publish_note(format_args!(
                "mqtt subscription to {topic} not made: {}",
                reason(&error)
            ));
            Ok(())
        }
        Err(error) => Err(error.into()),
    }
}

/// Publish one message at QoS 0.
///
/// A recoverable refusal by the client - a payload past the broker's packet
/// size, a broker without retain - is named in the capture stream and
/// swallowed: it says nothing about the health of the connection, and
/// dropping the session over it would turn into a reconnect loop.
async fn publish(
    client: &mut Mqtt<'_, '_>,
    name: TopicName<'_>,
    payload: &str,
    retain: bool,
) -> Result<(), Trouble> {
    let mut options = PublicationOptions::new(TopicReference::Name(name));
    if retain {
        options = options.retain();
    }
    match client
        .publish(&options, Bytes::from(payload.as_bytes()))
        .await
    {
        Ok(_) => {
            PUBLISHED.fetch_add(1, Ordering::Relaxed);
            Ok(())
        }
        Err(error) if error.is_recoverable() => {
            crate::publish_note(format_args!(
                "mqtt publish of {} bytes not sent: {}",
                payload.len(),
                reason(&error)
            ));
            Ok(())
        }
        Err(error) => Err(error.into()),
    }
}

/// Publish the retained discovery config of every entity.
///
/// Sent on every connect and whenever Home Assistant announces itself on
/// `homeassistant/status`, because a restarted HA with a cleared recorder
/// otherwise has no entities until the next reboot of this device.
async fn publish_discovery(client: &mut Mqtt<'_, '_>) -> Result<(), Trouble> {
    let mut name: String<TOPIC_MAX> = String::new();
    let mut payload: String<DISCOVERY_MAX> = String::new();

    for descriptor in entity::ENTITIES {
        name.clear();
        payload.clear();
        let built = entity::write_discovery_topic(&mut name, descriptor).is_ok()
            && entity::write_discovery(&mut payload, descriptor, env!("CARGO_PKG_VERSION")).is_ok();
        if !built {
            // A buffer too small is our bug, not the broker's; name the entity
            // instead of publishing a truncated config.
            crate::publish_note(format_args!(
                "mqtt discovery payload for {} does not fit {DISCOVERY_MAX} bytes",
                descriptor.object_id
            ));
            continue;
        }
        let Some(name) = topic_of(&name) else {
            crate::publish_note(format_args!(
                "mqtt discovery topic for {} is not a legal topic name",
                descriptor.object_id
            ));
            continue;
        };
        publish(client, name, &payload, true).await?;
        Timer::after(DISCOVERY_PACE).await;
    }
    Ok(())
}

/// Build the state document and publish it if it differs from the last one.
async fn publish_state(
    client: &mut Mqtt<'_, '_>,
    session: &mut Session,
    snapshot: Option<&Snapshot>,
) -> Result<(), Trouble> {
    let mut next: String<STATE_MAX> = String::new();
    let view = session.view(snapshot);
    if json::write_state(&mut next, &view).is_err() {
        crate::publish_note(format_args!(
            "mqtt state document does not fit {STATE_MAX} bytes"
        ));
        return Ok(());
    }
    // Compared without the counters, which grow on every poll: see
    // [`json::volatile_free`].
    if json::volatile_free(&next) == json::volatile_free(&session.published) {
        return Ok(());
    }
    publish(client, STATE, &next, true).await?;
    session.published = next;
    Ok(())
}

// ---------------------------------------------------------------------------
// Incoming commands
// ---------------------------------------------------------------------------

/// Handle one PUBLISH from the broker.
///
/// Anything that is not a valid command for a writable entity is logged into
/// the capture stream and dropped - never forwarded, never guessed at (ADR,
/// "invalid payloads are rejected and logged, never forwarded").
async fn on_message(
    client: &mut Mqtt<'_, '_>,
    session: &mut Session,
    snapshots: &mut master::SnapshotReceiver,
    message: &Message<'_>,
) -> Result<(), Trouble> {
    let Some(name) = message.topic.name() else {
        // A topic alias, which we told the broker in CONNECT we do not take.
        crate::publish_note(format_args!("mqtt message on an unexpected topic alias"));
        return Ok(());
    };
    let topic = name.as_ref().as_str();
    let payload = message.message.as_bytes();

    if topic == entity::TOPIC_HA_STATUS {
        if payload == entity::PAYLOAD_ONLINE.as_bytes() {
            crate::publish_note(format_args!("mqtt home assistant online: discovery again"));
            publish_discovery(client).await?;
            session.published.clear();
            publish_state(client, session, snapshots.try_get().as_ref()).await?;
        }
        return Ok(());
    }

    let Some(object) = entity::command_object(topic) else {
        crate::publish_note(format_args!("mqtt message on unexpected topic {topic}"));
        return Ok(());
    };
    RECEIVED.fetch_add(1, Ordering::Relaxed);

    let Ok(text) = core::str::from_utf8(payload) else {
        return refuse(
            client,
            session,
            snapshots,
            object,
            "<not utf-8>",
            "payload is not UTF-8",
        )
        .await;
    };

    // The command subscription asks the broker not to send the retained store
    // at all, so this catches a retained message published while we are up.
    // Home Assistant never publishes commands retained; anything that does is
    // almost certainly a stray `mosquitto_pub -r`.
    if message.retain {
        return refuse(
            client,
            session,
            snapshots,
            object,
            text,
            "retained command ignored",
        )
        .await;
    }
    let command = match entity::parse_set(object, text) {
        Ok(command) => command,
        Err(reason) => {
            return refuse(client, session, snapshots, object, text, reason).await;
        }
    };

    // Listen mode is the safety interlock: a command is refused outright
    // rather than queued, so it cannot fire the moment somebody switches the
    // controller to master.
    if master::mode() != OpMode::Master {
        return refuse(
            client,
            session,
            snapshots,
            object,
            text,
            "controller is in listen mode",
        )
        .await;
    }
    if !master::submit(command) {
        return refuse(
            client,
            session,
            snapshots,
            object,
            text,
            "command queue full",
        )
        .await;
    }

    let mut line = Line::new();
    if master::write_command(&mut line, &command).is_ok() {
        crate::publish_note(format_args!("mqtt command {line} queued"));
    }
    // No optimistic state: the entity moves when the heat pump's own readback
    // says so, which is what the next snapshot carries. The outcome arrives
    // on OUTCOMES and lands in the diagnostic sensor.
    Ok(())
}

/// Log a refused command, record it in the diagnostic sensor and publish it
/// now, as a bus outcome is: whoever sent the command is waiting for an
/// answer, and the next snapshot or refresh may be a minute away.
async fn refuse(
    client: &mut Mqtt<'_, '_>,
    session: &mut Session,
    snapshots: &mut master::SnapshotReceiver,
    object: &str,
    payload: &str,
    reason: &str,
) -> Result<(), Trouble> {
    DROPPED.fetch_add(1, Ordering::Relaxed);
    let clipped = clip(payload, CLIP);
    crate::publish_note(format_args!(
        "mqtt command {object}={clipped} refused: {reason}"
    ));
    let mut line = Line::new();
    let _ = write!(line, "{object} {clipped} -> {reason}");
    session.record(&line);
    publish_state(client, session, snapshots.try_get().as_ref()).await
}

/// At most `max` bytes, cut on a character boundary.
fn clip_bytes(text: &str, max: usize) -> &str {
    let mut end = text.len().min(max);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

/// At most `max` characters, cut on a character boundary.
fn clip(text: &str, max: usize) -> &str {
    match text.char_indices().nth(max) {
        Some((at, _)) => &text[..at],
        None => text,
    }
}

// ---------------------------------------------------------------------------
// Session state
// ---------------------------------------------------------------------------

/// What survives one iteration of the event loop: the document last published
/// (so a change can be detected) and the last command report with its number.
struct Session {
    published: String<STATE_MAX>,
    last_command: String<REPORT_MAX>,
    /// Number of the last report, from 1; survives reconnects, not reboots.
    seq: u32,
}

impl Session {
    fn new() -> Self {
        Self {
            published: String::new(),
            last_command: String::new(),
            seq: 0,
        }
    }

    /// Record a command report, in the same wording the line interface uses.
    fn set_report(&mut self, report: &CommandReport) {
        let mut line = Line::new();
        if master::write_report(&mut line, report).is_ok() {
            self.record(&line);
        }
    }

    /// Put `text` in the diagnostic sensor as `#<n> <text>`. The number makes
    /// every report change the state document, so a client can tell a second
    /// identical outcome (the same command refused twice) from no answer.
    fn record(&mut self, text: &str) {
        self.seq = self.seq.wrapping_add(1);
        self.last_command.clear();
        let _ = write!(self.last_command, "#{} ", self.seq);
        let room = REPORT_MAX - self.last_command.len();
        let _ = self.last_command.push_str(clip_bytes(text, room));
    }

    /// The pure view [`json::write_state`] builds the document from.
    fn view<'a>(&'a self, snapshot: Option<&'a Snapshot>) -> json::View<'a> {
        json::View {
            status: snapshot.and_then(|s| s.status.as_ref()),
            settings: snapshot.and_then(|s| s.settings.as_ref()),
            link_up: snapshot.is_some_and(|s| s.link == LinkState::Up),
            controller_mode: snapshot.map_or_else(master::mode, |s| s.mode).as_str(),
            last_command: &self.last_command,
            counters: snapshot.map_or_else(json::Counters::default, |s| json::Counters {
                requests: s.counters.requests,
                timeouts: s.counters.timeouts,
                writes: s.counters.writes,
                write_failures: s.counters.write_failures,
            }),
        }
    }
}
