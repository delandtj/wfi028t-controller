//! The line interface: the same commands on USB and on TCP 4000.
//!
//! The sniffer's commands (`bus`, `status`, `note`) come from
//! `modbus-sniffer-core` and behave exactly as they always did. Two
//! controller-only commands are handled here *before* the core parser is
//! consulted, so the core crate needs no changes:
//!
//! | Command | Effect |
//! |---|---|
//! | `mode` | report the current operating mode |
//! | `mode listen` | stop transmitting (always allowed) |
//! | `mode master` | listen first, then become the bus master |
//! | `set power on\|off` | queue a [`Command::SetPower`] |
//! | `set boost on\|off` | full power on/off |
//! | `set p05 on\|off` | stop once the target is reached |
//! | `set mode heat\|cool\|auto` | operating mode of the heat pump |
//! | `set p01..p04 <n>` | setpoints and hysteresis, whole degrees |
//! | `mqtt` | report the broker configuration and connection state |
//! | `mqtt host <ip> [port]` | point at a broker (persisted, overrides the build-time one) |
//! | `mqtt user <u> <p>` | broker credentials (persisted, never echoed back) |
//! | `mqtt off` | no MQTT, even if the image was built with a broker |
//!
//! `set` exists to drive the heat pump from a bench terminal before the MQTT
//! task is written; it goes through the same [`master::COMMANDS`] queue and the
//! same validation, and its reply is the real [`master::CommandOutcome`].
//!
//! The `status` line keeps the sniffer's fields and order and gains
//! `mode=`, `link=`, the bus-master counters and the `mqtt_*` fields at the
//! end, so the capture daemon and analyzer (which treat `# ` lines as opaque
//! status text) are unaffected. The MQTT password is never in it.

use core::fmt::Write as _;

use embassy_futures::select::{select, Either};
use embassy_time::{Duration, Instant, Timer};

use hp_model::settings::Mode;
use hp_model::Command as HpCommand;
use modbus_sniffer_core as sniffer;
use sniffer::{Command, Line};

use crate::master::{self, CommandReport, OpMode};
use crate::mqtt;

/// How long a `set` command waits for its outcome before answering "queued".
///
/// Three writes 500 ms apart plus the readback on the following settings poll
/// take about 2.5 s once the write has started, and the write starts at the
/// next settings slot (up to 1 s away).
const OUTCOME_WAIT: Duration = Duration::from_secs(6);

/// Reassembles LF- or CRLF-terminated command lines from a byte stream.
pub struct CommandBuffer {
    buf: [u8; sniffer::MAX_COMMAND_LEN],
    len: usize,
    /// Set when the line overran the buffer, so the whole line is rejected
    /// instead of being silently truncated into a different command.
    overflow: bool,
}

impl CommandBuffer {
    pub const fn new() -> Self {
        Self {
            buf: [0u8; sniffer::MAX_COMMAND_LEN],
            len: 0,
            overflow: false,
        }
    }

    /// Feed one received byte. Returns the reply line once a complete command
    /// has been handled.
    pub async fn feed(&mut self, byte: u8) -> Option<Line> {
        if byte != b'\r' && byte != b'\n' {
            if self.len == self.buf.len() {
                self.overflow = true;
            } else {
                self.buf[self.len] = byte;
                self.len += 1;
            }
            return None;
        }

        let len = self.len;
        let overflow = self.overflow;
        self.len = 0;
        self.overflow = false;

        if overflow {
            return Some(reply_err("command too long"));
        }
        if len == 0 {
            // The second half of a CRLF, or a blank line. Not a command.
            return None;
        }

        match core::str::from_utf8(&self.buf[..len]) {
            Ok(text) => Some(execute(text).await),
            Err(_) => Some(reply_err("command is not valid UTF-8")),
        }
    }
}

impl Default for CommandBuffer {
    fn default() -> Self {
        Self::new()
    }
}

fn reply_err(reason: &str) -> Line {
    let mut reply = Line::new();
    if sniffer::format_command_err(reason, &mut reply).is_err() {
        reply.clear();
        let _ = reply.push_str("# err reply too long\r\n");
    }
    reply
}

fn reply_ok(args: core::fmt::Arguments<'_>) -> Line {
    let mut reply = Line::new();
    if write!(reply, "# ok ").is_err() || reply.write_fmt(args).is_err() {
        return reply_err("reply too long");
    }
    if reply.push_str("\r\n").is_err() {
        return reply_err("reply too long");
    }
    reply
}

/// Parse and run one command line, returning the reply for its requester.
pub async fn execute(text: &str) -> Line {
    let trimmed = text.trim_matches([' ', '\t', '\r', '\n']);
    let (keyword, rest) = match trimmed.find([' ', '\t']) {
        Some(i) => (&trimmed[..i], trimmed[i + 1..].trim_matches([' ', '\t'])),
        None => (trimmed, ""),
    };

    // Controller-only commands first; everything else falls through to the
    // sniffer's parser.
    if keyword.eq_ignore_ascii_case("mode") {
        return mode_command(rest).await;
    }
    if keyword.eq_ignore_ascii_case("set") {
        return set_command(rest).await;
    }
    if keyword.eq_ignore_ascii_case("mqtt") {
        return mqtt_command(rest).await;
    }

    let command = match sniffer::parse_command(text) {
        Ok(command) => command,
        Err(reason) => {
            // The core parser's "unknown command" list does not know about the
            // two commands above; say the whole list.
            if reason.starts_with("unknown command") {
                return reply_err("unknown command (bus, status, note, mode, set, mqtt)");
            }
            return reply_err(reason);
        }
    };

    match command {
        Command::Status => status_reply(),

        Command::Note(text) => {
            let mut line = Line::new();
            if sniffer::format_note(Instant::now().as_millis(), text, &mut line).is_ok() {
                crate::publish_text(&line);
            }
            let mut reply = Line::new();
            if sniffer::format_command_ok(&command, &mut reply).is_err() {
                return reply_err("note too long");
            }
            reply
        }

        Command::Bus(new_bus) => {
            // Apply first: the live effect is the point of the command. The
            // bus task picks the change up and re-applies the UART
            // configuration; in a selftest build nobody is listening, which is
            // harmless.
            crate::set_bus(new_bus);
            crate::request_bus_change(new_bus);

            // Then persist. A board with no settings partition still honours
            // the change for this session and says that it did not stick.
            if let Err(reason) = crate::settings::store_bus(new_bus).await {
                return reply_err(reason);
            }
            let mut reply = Line::new();
            if sniffer::format_command_ok(&command, &mut reply).is_err() {
                return reply_err("reply too long");
            }
            reply
        }
    }
}

// ---------------------------------------------------------------------------
// status
// ---------------------------------------------------------------------------

/// The sniffer's status line with the bus master's state appended.
fn status_reply() -> Line {
    use core::sync::atomic::Ordering;

    let status = sniffer::Status {
        uptime_ms: Instant::now().as_millis(),
        bus: crate::current_bus(),
        wifi_up: crate::WIFI_UP.load(Ordering::Relaxed),
        ip: match crate::WIFI_IP.load(Ordering::Relaxed) {
            0 => None,
            raw => Some(raw.to_be_bytes()),
        },
        rssi: match crate::WIFI_RSSI.load(Ordering::Relaxed) {
            crate::RSSI_UNKNOWN => None,
            rssi => Some(rssi),
        },
        frames: crate::frames(),
        bad_crc: crate::bad_crc(),
        uart_errors: crate::uart_errors(),
        dropped: crate::dropped_lines(),
        usb_dropped: crate::usb_dropped_lines(),
    };

    let mut reply = Line::new();
    if sniffer::format_status(&status, &mut reply).is_err() {
        return reply_err("status line too long");
    }
    // Drop the CRLF the core formatter appended, add our fields, put it back.
    while reply.ends_with('\r') || reply.ends_with('\n') {
        reply.pop();
    }

    let now_ms = Instant::now().as_millis();
    let snapshot = master::SNAPSHOT.try_get();
    let link = snapshot.map_or(master::LinkState::Down, |s| s.link);
    let counters = snapshot.map(|s| s.counters).unwrap_or_default();
    let settings_age = snapshot
        .map(|s| s.settings_age_ms(now_ms))
        .unwrap_or(u32::MAX);

    let appended = write!(
        reply,
        " mode={} link={} requests={} status_ok={} settings_ok={} timeouts={} \
         bad_responses={} echoes={} foreign={} writes={} write_failures={} \
         settings_age_ms={}",
        snapshot.map_or_else(master::mode, |s| s.mode).as_str(),
        link.as_str(),
        counters.requests,
        counters.status_ok,
        counters.settings_ok,
        counters.timeouts,
        counters.bad_responses,
        counters.echoes,
        counters.foreign,
        counters.writes,
        counters.write_failures,
        settings_age,
    );
    if appended.is_err() {
        return reply_err("status line too long");
    }

    // A little of the decoded state, so a bench session can see the heat pump
    // without a second tool. Temperatures are in tenths of a degree (`_dc`),
    // because that is what the model carries and it never needs a decimal
    // point here.
    if let Some(snapshot) = snapshot {
        if let Some(settings) = snapshot.settings {
            let _ = write!(
                reply,
                " hp_power={} hp_boost={} hp_mode={} hp_setpoint={}",
                u8::from(settings.power()),
                u8::from(settings.boost()),
                match settings.mode() {
                    Mode::Heat => "heat",
                    Mode::Cool => "cool",
                    Mode::Auto => "auto",
                    Mode::Other(_) => "other",
                },
                settings.active_setpoint().unwrap_or(0),
            );
        }
        if let Some(status) = snapshot.status {
            let _ = write!(
                reply,
                " hp_inlet_dc={} hp_outlet_dc={} hp_hz={} hp_fault={}",
                status.inlet_water().map_or(0, |t| t.tenths()),
                status.outlet_water().map_or(0, |t| t.tenths()),
                status.compressor_hz().unwrap_or(0),
                u8::from(status.water_flow_fault()),
            );
        }
        let _ = write!(
            reply,
            " status_age_ms={} snapshot_age_ms={}",
            now_ms.saturating_sub(snapshot.status_ms),
            now_ms.saturating_sub(snapshot.updated_ms),
        );
    }

    // MQTT last, appended the way the mode field was: the host parser reads
    // `# ` lines as opaque text, so new fields at the end are free. The
    // password is deliberately not among them.
    let _ = write!(reply, " mqtt={}", mqtt::connection().as_str());
    match mqtt::effective() {
        Some(broker) => {
            let [a, b, c, d] = broker.host();
            let _ = write!(
                reply,
                " mqtt_host={a}.{b}.{c}.{d}:{} mqtt_user={}",
                broker.port(),
                if broker.user().is_empty() {
                    "-"
                } else {
                    broker.user()
                },
            );
        }
        None => {
            let _ = write!(reply, " mqtt_host=- mqtt_user=-");
        }
    }
    let _ = write!(
        reply,
        " mqtt_published={} mqtt_received={} mqtt_dropped={} mqtt_failures={}",
        mqtt::published(),
        mqtt::received(),
        mqtt::dropped(),
        mqtt::failures(),
    );

    // The two ADR 0002 fields, at the end for the same reason the mqtt ones
    // are: a host parser reads `# ` lines as opaque text. `ota=` is the
    // running image's OTA verdict (`pending` while it is on probation), and
    // `console=` says whether somebody is attached to port 4001.
    let _ = write!(
        reply,
        " ota={} console={}",
        crate::ota::state_str(),
        crate::net::console_state(),
    );

    if reply.push_str("\r\n").is_err() {
        return reply_err("status line too long");
    }
    reply
}

// ---------------------------------------------------------------------------
// mode
// ---------------------------------------------------------------------------

async fn mode_command(rest: &str) -> Line {
    if rest.is_empty() {
        return reply_ok(format_args!("mode {}", master::mode().as_str()));
    }
    let Some(wanted) = OpMode::parse(rest) else {
        return reply_err("usage: mode [listen|master]");
    };

    match master::request_mode(wanted).await {
        Ok(now) => {
            // Remember the operator's intent. An automatic fallback to listen
            // (a foreign master appearing) deliberately does NOT persist: after
            // a reboot the silence check runs again and decides afresh.
            if let Err(reason) = crate::settings::store_mode(now).await {
                return reply_err(reason);
            }
            reply_ok(format_args!("mode {}", now.as_str()))
        }
        Err(reason) => reply_err(reason),
    }
}

// ---------------------------------------------------------------------------
// mqtt
// ---------------------------------------------------------------------------

/// `mqtt [host <ip> [port] | user <name> <password> | off]`.
///
/// A change takes effect at once (the MQTT task drops the connection and
/// reconnects) and is then persisted, like the `bus` command: a board with no
/// settings partition still honours it for this session and says that it did
/// not stick. The password is never echoed, here or in `status`.
async fn mqtt_command(rest: &str) -> Line {
    let request = match mqtt::config::parse_request(rest) {
        Ok(request) => request,
        Err(reason) => return reply_err(reason),
    };

    let next = match request {
        mqtt::Request::Report => return mqtt_report(),
        mqtt::Request::Off => mqtt::Stored::Off,
        mqtt::Request::Address(host, port) => {
            // Keep whatever credentials are in force, so `mqtt host` after
            // `mqtt user` does not silently drop the password.
            let broker = match mqtt::effective() {
                Some(current) => current.with_address(host, port),
                None => match mqtt::Broker::new(host, port, "", "") {
                    Some(broker) => broker,
                    None => return reply_err("broker configuration rejected"),
                },
            };
            mqtt::Stored::On(broker)
        }
        mqtt::Request::Credentials(user, pass) => {
            let Some(current) = mqtt::effective() else {
                return reply_err("no broker address yet (try: mqtt host <ip>)");
            };
            match current.with_credentials(user, pass) {
                Some(broker) => mqtt::Stored::On(broker),
                None => return reply_err("credentials too long"),
            }
        }
    };

    mqtt::configure(next);
    if let Err(reason) = crate::settings::store_broker(next).await {
        return reply_err(reason);
    }
    mqtt_report()
}

/// The broker configuration and connection state, without the password.
fn mqtt_report() -> Line {
    // Where the configuration in force comes from: the image, or this
    // command (which is what the flash record holds).
    let source = if matches!(mqtt::stored(), mqtt::Stored::Unset) {
        "build"
    } else {
        "command"
    };
    let mut line = Line::new();
    match mqtt::effective() {
        Some(broker) => {
            let [a, b, c, d] = broker.host();
            let _ = write!(
                line,
                "mqtt {} host={a}.{b}.{c}.{d}:{} user={} password={} source={source}",
                mqtt::connection().as_str(),
                broker.port(),
                if broker.user().is_empty() {
                    "-"
                } else {
                    broker.user()
                },
                if broker.pass_opt().is_some() {
                    "set"
                } else {
                    "unset"
                },
            );
        }
        None => {
            let _ = write!(
                line,
                "mqtt {} host=- source={source}",
                mqtt::connection().as_str()
            );
        }
    }
    reply_ok(format_args!("{line}"))
}

// ---------------------------------------------------------------------------
// set
// ---------------------------------------------------------------------------

/// `set <field> <value>` -> one [`HpCommand`], then wait for its outcome.
async fn set_command(rest: &str) -> Line {
    let (field, value) = match rest.find([' ', '\t']) {
        Some(i) => (&rest[..i], rest[i + 1..].trim_matches([' ', '\t'])),
        None => (rest, ""),
    };

    let command = match parse_set(field, value) {
        Ok(command) => command,
        Err(usage) => return reply_err(usage),
    };

    if master::mode() != OpMode::Master {
        return reply_err("not in master mode (try: mode master)");
    }

    // An update has been accepted and the reboot is a few hundred
    // milliseconds away: this command would be queued into a firmware that
    // is about to stop existing. Probation itself does NOT block commands
    // (ADR 0002, review ask 1) - only the reboot does.
    if crate::ota::rebooting() {
        return reply_err("rebooting into a new image; re-issue this after the reboot");
    }

    // Subscribe before queueing, and mark whatever is already there as seen,
    // so the reply cannot be a previous command's outcome.
    let mut receiver = master::OUTCOMES.receiver();
    if let Some(receiver) = receiver.as_mut() {
        let _ = receiver.try_changed();
    }

    if !master::submit(command) {
        return reply_err("command queue full");
    }

    let Some(receiver) = receiver.as_mut() else {
        return reply_ok(format_args!("set queued (no outcome slot free)"));
    };

    let wait = async {
        loop {
            let report: CommandReport = receiver.changed().await;
            if report.command == command {
                return report;
            }
        }
    };

    let mut line = Line::new();
    match select(wait, Timer::after(OUTCOME_WAIT)).await {
        Either::First(report) => {
            if master::write_report(&mut line, &report).is_err() {
                return reply_err("reply too long");
            }
            reply_ok(format_args!("set {line}"))
        }
        Either::Second(()) => {
            if master::write_command(&mut line, &command).is_err() {
                return reply_err("reply too long");
            }
            reply_ok(format_args!("set {line} -> queued (outcome follows)"))
        }
    }
}

fn parse_set(field: &str, value: &str) -> Result<HpCommand, &'static str> {
    const USAGE: &str = "usage: set power|boost|p05 on|off, set mode heat|cool|auto, \
                         set p01|p02|p03|p04 <degrees>";

    let as_bool = || match value {
        v if v.eq_ignore_ascii_case("on") || v == "1" || v.eq_ignore_ascii_case("true") => {
            Some(true)
        }
        v if v.eq_ignore_ascii_case("off") || v == "0" || v.eq_ignore_ascii_case("false") => {
            Some(false)
        }
        _ => None,
    };
    let as_u8 = || value.parse::<u8>().ok();

    if field.eq_ignore_ascii_case("power") {
        return as_bool().map(HpCommand::SetPower).ok_or(USAGE);
    }
    if field.eq_ignore_ascii_case("boost") {
        return as_bool().map(HpCommand::SetBoost).ok_or(USAGE);
    }
    if field.eq_ignore_ascii_case("p05") {
        return as_bool().map(HpCommand::SetStopAtTarget).ok_or(USAGE);
    }
    if field.eq_ignore_ascii_case("mode") {
        let mode = if value.eq_ignore_ascii_case("heat") {
            Mode::Heat
        } else if value.eq_ignore_ascii_case("cool") {
            Mode::Cool
        } else if value.eq_ignore_ascii_case("auto") {
            Mode::Auto
        } else {
            return Err(USAGE);
        };
        return Ok(HpCommand::SetMode(mode));
    }
    if field.eq_ignore_ascii_case("p01") {
        return as_u8().map(HpCommand::SetHeatSetpoint).ok_or(USAGE);
    }
    if field.eq_ignore_ascii_case("p02") {
        return as_u8().map(HpCommand::SetCoolSetpoint).ok_or(USAGE);
    }
    if field.eq_ignore_ascii_case("p03") {
        return as_u8().map(HpCommand::SetAutoSetpoint).ok_or(USAGE);
    }
    if field.eq_ignore_ascii_case("p04") {
        return as_u8().map(HpCommand::SetHysteresis).ok_or(USAGE);
    }
    Err(USAGE)
}
