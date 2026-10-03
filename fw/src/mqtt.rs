//! MQTT 3.1.1 client and Home Assistant discovery: the ADR's component 3.
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
//! # Why a hand-rolled client
//!
//! `rust-mqtt` was the ADR's default. As of 0.6.0 its `v3` module is an empty
//! placeholder - the crate is MQTT 5 only - and the v5 client needs `alloc`
//! or its bump allocator plus a session state machine for the QoS 1/2 flows
//! we do not use. The last version with a working 3.1.1 client, 0.3.0, is
//! built on `embedded-io-async` 0.6, which does not match embassy-net 0.9's
//! sockets (0.7). `minimq` 0.13 does match our dependency versions exactly
//! but is also MQTT 5 only and pulls in `serde`. What is actually needed here
//! is CONNECT/CONNACK, PUBLISH at QoS 0 with retain, SUBSCRIBE and PINGREQ:
//! [`proto`] is that, in 300 lines of pure functions with host tests, no new
//! dependencies and no allocator.
//!
//! # Where the pieces live
//!
//! | Module | Job |
//! |---|---|
//! | [`proto`] | packet encode/decode, pure |
//! | [`entity`] | the entity table, discovery payloads, payload parsing, pure |
//! | [`json`] | the state document, pure |
//! | [`config`] | broker address and credentials, flash record, `mqtt` command, pure |
//! | this file | the socket, the reconnect loop and the event loop |

use core::cell::Cell;
use core::fmt::Write as _;
use core::sync::atomic::{AtomicU32, AtomicU8, Ordering};

use embassy_executor::Spawner;
use embassy_futures::select::{select, select4, Either, Either4};
use embassy_net::tcp::TcpSocket;
use embassy_net::{IpEndpoint, Ipv4Address, Stack};
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::blocking_mutex::Mutex as BlockingMutex;
use embassy_sync::signal::Signal;
use embassy_time::{with_timeout, Duration, Instant, Timer};
use embedded_io_async::Write as _;
use heapless::String;
use static_cell::StaticCell;

use modbus_sniffer_core::Line;

use crate::master::{self, CommandReport, LinkState, OpMode, Snapshot};

pub mod config;
pub mod entity;
pub mod json;
pub mod proto;

pub use config::{Broker, Request, Stored};

/// Keepalive promised to the broker in CONNECT.
const KEEPALIVE_S: u16 = 60;

/// How often a PINGREQ goes out while the connection is otherwise idle. Well
/// inside [`KEEPALIVE_S`], so a slow broker cannot make us look dead.
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

/// Socket buffers. RX only ever carries CONNACK, SUBACK, PINGRESP and short
/// command payloads; TX has to hold one discovery payload plus its topic.
const SOCKET_RX: usize = 512;
const SOCKET_TX: usize = 1024;

/// Packet reassembly buffer. Everything we subscribe to is a handful of
/// bytes; anything bigger is read and thrown away by [`Incoming`] rather
/// than breaking the connection, so this does not have to be generous.
const INCOMING_MAX: usize = 256;

/// Buffer for the state document. The host test
/// `json::tests::the_document_fits_the_task_buffer` holds this size.
const STATE_MAX: usize = 1024;

/// Buffer for one discovery payload, likewise bounded by a host test.
const DISCOVERY_MAX: usize = 640;

/// Buffer for a topic.
const TOPIC_MAX: usize = 64;

/// How much of the last command report the diagnostic sensor carries.
const REPORT_MAX: usize = 96;

/// Characters of a bad payload echoed into the capture stream.
const CLIP: usize = 24;

/// Largest CONNECT we can build: 10 bytes of variable header, the client id,
/// the will topic and payload, and the longest credentials, plus the five
/// bytes [`proto`] reserves for the fixed header. 150 bytes today.
const CONNECT_MAX: usize = 192;

/// The two subscriptions, hence the SUBSCRIBE buffer.
const SUBSCRIBE_MAX: usize = 64;

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
        // broker that stops acknowledging while we are inside a `write_all`
        // (a full TX buffer) would otherwise park this task for good, and the
        // ping arm of the event loop never gets to run. Probing every
        // [`PING_INTERVAL`] keeps a healthy idle connection well inside it.
        socket.set_timeout(Some(SOCKET_TIMEOUT));
        socket.set_keep_alive(Some(PING_INTERVAL));

        // Only a reconfiguration ends a session cleanly.
        let trouble = session_with(
            &mut socket,
            &broker,
            &mut session,
            &mut snapshots,
            &mut outcomes,
        )
        .await
        .err();
        socket.abort();
        let _ = socket.flush().await;
        drop(socket);

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
                    "mqtt {} ({a}.{b}.{c}.{d}:{}), retry in {} s",
                    trouble.as_str(),
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
    /// The broker refused the CONNECT (return code in the payload).
    Refused(u8),
    /// The broker said something we cannot parse, or something too big for
    /// the receive buffer.
    Protocol,
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
            Self::Refused(_) => "connection refused by broker",
            Self::Protocol => "protocol error",
            Self::SocketClosed => "connection lost",
            Self::NoPingResponse => "broker stopped answering pings",
        }
    }
}

impl From<proto::Error> for Trouble {
    fn from(error: proto::Error) -> Self {
        match error {
            proto::Error::ConnectionRefused(code) => Self::Refused(code),
            _ => Self::Protocol,
        }
    }
}

impl From<embassy_net::tcp::Error> for Trouble {
    fn from(_: embassy_net::tcp::Error) -> Self {
        Self::SocketClosed
    }
}

/// Connect, announce, and serve until something breaks.
///
/// `Ok(())` means the configuration changed and the caller should start over;
/// every other ending is a [`Trouble`].
async fn session_with(
    socket: &mut TcpSocket<'_>,
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

    let (mut reader, mut writer) = socket.split();
    let mut rx = Incoming::new();

    // CONNECT, with the availability topic as the last will so the broker
    // marks the device offline if this connection dies without a goodbye.
    let mut packet = [0u8; CONNECT_MAX];
    let len = proto::encode_connect(
        &mut packet,
        &proto::Connect {
            client_id: entity::DEVICE_ID,
            keepalive_s: KEEPALIVE_S,
            user: broker.user_opt(),
            pass: broker.pass_opt(),
            will: Some(proto::Will {
                topic: entity::TOPIC_AVAILABILITY,
                payload: entity::PAYLOAD_OFFLINE,
                retain: true,
            }),
        },
    )?;
    writer.write_all(&packet[..len]).await?;
    writer.flush().await?;

    // CONNACK before anything else, as the protocol requires. The result is
    // bound before it is matched, so the borrow of `rx` taken by the read
    // future is over by the time the body is looked at.
    let connack = with_timeout(CONNACK_TIMEOUT, rx.next(&mut reader)).await;
    match connack {
        Ok(Ok(packet)) => {
            if proto::PacketType::from_header(packet.header) != proto::PacketType::Connack {
                return Err(Trouble::Protocol);
            }
            proto::parse_connack(rx.body(packet.body))?;
        }
        Ok(Err(trouble)) => return Err(trouble),
        Err(_) => return Err(Trouble::Timeout),
    }
    rx.consume();

    let [a, b, c, d] = broker.host();
    crate::publish_note(format_args!(
        "mqtt connected to {a}.{b}.{c}.{d}:{}, publishing {} entities",
        broker.port(),
        entity::ENTITIES.len()
    ));

    publish(
        &mut writer,
        entity::TOPIC_AVAILABILITY,
        entity::PAYLOAD_ONLINE,
        true,
    )
    .await?;
    publish_discovery(&mut writer).await?;

    let mut subscribe = [0u8; SUBSCRIBE_MAX];
    let len = proto::encode_subscribe(
        &mut subscribe,
        1,
        &[entity::TOPIC_COMMAND_FILTER, entity::TOPIC_HA_STATUS],
    )?;
    writer.write_all(&subscribe[..len]).await?;
    writer.flush().await?;

    // First state document of this connection, unconditionally.
    session.published.clear();
    publish_state(&mut writer, session, snapshots.try_get().as_ref()).await?;

    let mut refresh_at = Instant::now() + REFRESH;
    let mut ping_at = Instant::now() + PING_INTERVAL;
    let mut awaiting_pong = false;

    set_connection(Connection::Online);

    loop {
        let deadline = refresh_at.min(ping_at);
        let event = select4(
            rx.next(&mut reader),
            snapshots.changed(),
            outcomes.changed(),
            select(Timer::at(deadline), RECONFIGURED.wait()),
        )
        .await;

        match event {
            // A packet from the broker.
            Either4::First(packet) => {
                let Packet { header, body } = packet?;
                match proto::PacketType::from_header(header) {
                    proto::PacketType::Publish => {
                        let (topic, payload) = proto::split_publish(header, rx.body(body))?;
                        // Borrowed out of the receive buffer, so the command
                        // is handled before the buffer is reused.
                        let retained = header & 0x01 != 0;
                        on_message(&mut writer, session, snapshots, topic, payload, retained)
                            .await?;
                    }
                    proto::PacketType::Pingresp => awaiting_pong = false,
                    // SUBACK and anything else needs no action; a broker that
                    // refuses the subscription shows up as commands never
                    // arriving, which the capture log makes visible.
                    _ => {}
                }
                rx.consume();
            }

            // New heat pump state: publish only if the document changed.
            Either4::Second(snapshot) => {
                publish_state(&mut writer, session, Some(&snapshot)).await?;
            }

            // A command outcome: into the diagnostic sensor, then publish.
            Either4::Third(report) => {
                session.set_report(&report);
                publish_state(&mut writer, session, snapshots.try_get().as_ref()).await?;
            }

            // The refresh/keepalive timer.
            Either4::Fourth(Either::First(())) => {
                let now = Instant::now();
                if now >= refresh_at {
                    refresh_at = now + REFRESH;
                    session.published.clear();
                    publish_state(&mut writer, session, snapshots.try_get().as_ref()).await?;
                }
                if now >= ping_at {
                    if awaiting_pong {
                        return Err(Trouble::NoPingResponse);
                    }
                    ping_at = now + PING_INTERVAL;
                    awaiting_pong = true;
                    writer.write_all(&proto::PINGREQ).await?;
                    writer.flush().await?;
                }
            }
            // The `mqtt` command changed the configuration.
            Either4::Fourth(Either::Second(())) => {
                // Say goodbye properly: a retained "offline" and a DISCONNECT,
                // so the broker does not fire the will and HA sees one clean
                // transition.
                let _ = publish(
                    &mut writer,
                    entity::TOPIC_AVAILABILITY,
                    entity::PAYLOAD_OFFLINE,
                    true,
                )
                .await;
                let _ = writer.write_all(&proto::DISCONNECT).await;
                let _ = writer.flush().await;
                return Ok(());
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Publishing
// ---------------------------------------------------------------------------

type Writer<'a> = embassy_net::tcp::TcpWriter<'a>;
type Reader<'a> = embassy_net::tcp::TcpReader<'a>;

/// Publish one message at QoS 0.
///
/// The payload is written straight from the caller's buffer: only the fixed
/// header is assembled here, so a 600-byte discovery payload is never copied.
async fn publish(
    writer: &mut Writer<'_>,
    topic: &str,
    payload: &str,
    retain: bool,
) -> Result<(), Trouble> {
    let mut head = [0u8; proto::MAX_PUBLISH_HEADER];
    let len = proto::publish_header(&mut head, topic, payload.len(), retain)?;
    writer.write_all(&head[..len]).await?;
    writer.write_all(topic.as_bytes()).await?;
    writer.write_all(payload.as_bytes()).await?;
    writer.flush().await?;
    PUBLISHED.fetch_add(1, Ordering::Relaxed);
    Ok(())
}

/// Publish the retained discovery config of every entity.
///
/// Sent on every connect and whenever Home Assistant announces itself on
/// `homeassistant/status`, because a restarted HA with a cleared recorder
/// otherwise has no entities until the next reboot of this device.
async fn publish_discovery(writer: &mut Writer<'_>) -> Result<(), Trouble> {
    let mut topic: String<TOPIC_MAX> = String::new();
    let mut payload: String<DISCOVERY_MAX> = String::new();

    for descriptor in entity::ENTITIES {
        topic.clear();
        payload.clear();
        let built = entity::write_discovery_topic(&mut topic, descriptor).is_ok()
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
        publish(writer, &topic, &payload, true).await?;
        Timer::after(DISCOVERY_PACE).await;
    }
    Ok(())
}

/// Build the state document and publish it if it differs from the last one.
async fn publish_state(
    writer: &mut Writer<'_>,
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
    publish(writer, entity::TOPIC_STATE, &next, true).await?;
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
    writer: &mut Writer<'_>,
    session: &mut Session,
    snapshots: &mut master::SnapshotReceiver,
    topic: &str,
    payload: &[u8],
    retained: bool,
) -> Result<(), Trouble> {
    if topic == entity::TOPIC_HA_STATUS {
        if payload == entity::PAYLOAD_ONLINE.as_bytes() {
            crate::publish_note(format_args!("mqtt home assistant online: discovery again"));
            publish_discovery(writer).await?;
            session.published.clear();
            publish_state(writer, session, snapshots.try_get().as_ref()).await?;
        }
        return Ok(());
    }

    let Some(object) = entity::command_object(topic) else {
        crate::publish_note(format_args!("mqtt message on unexpected topic {topic}"));
        return Ok(());
    };
    RECEIVED.fetch_add(1, Ordering::Relaxed);

    let Ok(text) = core::str::from_utf8(payload) else {
        refuse(session, object, "<not utf-8>", "payload is not UTF-8");
        return Ok(());
    };

    // A retained command would be re-delivered on every single reconnect and
    // re-apply itself to the heat pump long after whoever published it meant
    // it. Home Assistant never publishes commands retained; anything that
    // does is almost certainly a stray `mosquitto_pub -r`.
    if retained {
        refuse(session, object, text, "retained command ignored");
        return Ok(());
    }
    let command = match entity::parse_set(object, text) {
        Ok(command) => command,
        Err(reason) => {
            refuse(session, object, text, reason);
            return Ok(());
        }
    };

    // Listen mode is the safety interlock: a command is refused outright
    // rather than queued, so it cannot fire the moment somebody switches the
    // controller to master.
    if master::mode() != OpMode::Master {
        refuse(session, object, text, "controller is in listen mode");
        return Ok(());
    }
    if !master::submit(command) {
        refuse(session, object, text, "command queue full");
        return Ok(());
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

/// Log a refused command and record it in the diagnostic sensor.
fn refuse(session: &mut Session, object: &str, payload: &str, reason: &str) {
    DROPPED.fetch_add(1, Ordering::Relaxed);
    let clipped = clip(payload, CLIP);
    crate::publish_note(format_args!(
        "mqtt command {object}={clipped} refused: {reason}"
    ));
    session.last_command.clear();
    let _ = write!(session.last_command, "{object} {clipped} -> {reason}");
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
/// (so a change can be detected) and the last command report.
struct Session {
    published: String<STATE_MAX>,
    last_command: String<REPORT_MAX>,
}

impl Session {
    fn new() -> Self {
        Self {
            published: String::new(),
            last_command: String::new(),
        }
    }

    /// Record a command report, in the same wording the line interface uses.
    fn set_report(&mut self, report: &CommandReport) {
        let mut line = Line::new();
        self.last_command.clear();
        if master::write_report(&mut line, report).is_ok() {
            let _ = self.last_command.push_str(clip(&line, REPORT_MAX));
        }
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

// ---------------------------------------------------------------------------
// Incoming packet buffer
// ---------------------------------------------------------------------------

/// One packet, as offsets into [`Incoming`]'s buffer.
struct Packet {
    header: u8,
    body: (usize, usize),
}

/// Reassembles packets from the TCP stream.
///
/// A single `read` is the only thing awaited, and it is cancel-safe: the
/// event loop can lose the race to a snapshot or a timer without losing
/// bytes, which a multi-read "read the whole packet" helper could not
/// promise.
struct Incoming {
    buf: [u8; INCOMING_MAX],
    len: usize,
    /// Bytes of the packet last returned, to be dropped on [`Self::consume`].
    done: usize,
    /// Bytes of an oversized packet still to be read and thrown away.
    discard: usize,
}

impl Incoming {
    const fn new() -> Self {
        Self {
            buf: [0; INCOMING_MAX],
            len: 0,
            done: 0,
            discard: 0,
        }
    }

    /// The body of a packet [`Self::next`] returned.
    fn body(&self, body: (usize, usize)) -> &[u8] {
        &self.buf[body.0..body.1]
    }

    /// Drop the packet last returned. Must be called before the next
    /// [`Self::next`], which is why the two are never in one expression.
    fn consume(&mut self) {
        self.buf.copy_within(self.done..self.len, 0);
        self.len -= self.done;
        self.done = 0;
    }

    /// The next whole packet, reading from the socket as needed.
    async fn next(&mut self, reader: &mut Reader<'_>) -> Result<Packet, Trouble> {
        loop {
            // Throw away an oversized packet rather than giving up on the
            // connection: the stream stays framed, so one absurd retained
            // message cannot turn into a reconnect loop.
            if self.discard > 0 {
                let want = self.discard.min(self.buf.len());
                match reader.read(&mut self.buf[..want]).await {
                    Ok(0) | Err(_) => return Err(Trouble::SocketClosed),
                    Ok(n) => self.discard -= n,
                }
                continue;
            }
            if let Some(packet) = self.parse()? {
                return Ok(packet);
            }
            if self.len == self.buf.len() {
                // Cannot happen: `parse` turns a packet larger than the
                // buffer into a discard before it can fill up. Belt and
                // braces against a silent spin.
                return Err(Trouble::Protocol);
            }
            match reader.read(&mut self.buf[self.len..]).await {
                Ok(0) => return Err(Trouble::SocketClosed),
                Ok(n) => self.len += n,
                Err(_) => return Err(Trouble::SocketClosed),
            }
        }
    }

    /// A whole packet in the buffer, if there is one.
    fn parse(&mut self) -> Result<Option<Packet>, Trouble> {
        let Some(&header) = self.buf[..self.len].first() else {
            return Ok(None);
        };
        let Some((remaining, digits)) = proto::decode_varint(&self.buf[1..self.len])? else {
            return Ok(None);
        };
        let start = 1 + digits;
        let end = start + remaining as usize;
        if end > self.buf.len() {
            // Nothing we subscribe to is anywhere near this big.
            crate::publish_note(format_args!(
                "mqtt dropped an oversized packet ({end} bytes, buffer {INCOMING_MAX})"
            ));
            self.discard = end - self.len;
            self.len = 0;
            self.done = 0;
            return Ok(None);
        }
        if self.len < end {
            return Ok(None);
        }
        self.done = end;
        Ok(Some(Packet {
            header,
            body: (start, end),
        }))
    }
}
