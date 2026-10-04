//! The entity table, the Home Assistant discovery payloads, and the parsing
//! of command payloads. Pure: no I/O, no clock, no allocation.
//!
//! One row per entity, from the "Target" section of `docs/register-map.md`.
//! Rows exist only for registers that section marks **confirmed** or
//! **likely** (plus the two derived ones it names), so an entity never
//! promises a value we cannot actually read. Entities "to map" there - manual
//! defrost, defrosting, any-fault, the error text - are deliberately absent.
//!
//! Every entity reads out of the one retained state document on
//! [`TOPIC_STATE`] through a `value_template`, so the device publishes one
//! JSON doc instead of 30 topics. [`Entity::object_id`] is also the key in
//! that document and the middle part of the command topic, which is what
//! makes [`parse_set`] a lookup rather than a second table.
//!
//! That same id goes out as Home Assistant's `object_id`, so an entity lands
//! at `<component>.wfi028t_<object_id>` instead of at a slug of its display
//! name. Entity ids then follow the register map, and editing a `name` below
//! no longer renames the entity a dashboard or an automation refers to.
//!
//! No `climate` entity. It was considered (ADR component 3) and does not map
//! cleanly: a single MQTT climate entity has one `min_temp`/`max_temp` pair,
//! while this machine has three setpoints with two different ranges (P01/P03
//! 8-40, P02 8-28); and `mode: off` would have to become two separate
//! commands (power off, then a mode write), which this firmware applies one
//! per 1 s cycle and reports as two outcomes. The plain switch, select and
//! number entities below express the machine exactly; a thermostat card can
//! be built HA-side from them.

use hp_model::settings::Mode;
use hp_model::Command;

/// Device identifier: the MQTT client id, the topic base and the HA device id.
pub const DEVICE_ID: &str = "wfi028t";

/// Retained state document, one JSON object, every entity's `state_topic`.
pub const TOPIC_STATE: &str = "wfi028t/state";

/// Retained availability topic, also the last will.
pub const TOPIC_AVAILABILITY: &str = "wfi028t/availability";

/// Subscription that covers every `wfi028t/<entity>/set`.
pub const TOPIC_COMMAND_FILTER: &str = "wfi028t/+/set";

/// Home Assistant's own birth/will topic, watched so discovery can be
/// republished when HA restarts.
pub const TOPIC_HA_STATUS: &str = "homeassistant/status";

/// Payload on [`TOPIC_AVAILABILITY`] while the controller is connected.
pub const PAYLOAD_ONLINE: &str = "online";

/// Payload on [`TOPIC_AVAILABILITY`] after a clean or dirty disconnect.
pub const PAYLOAD_OFFLINE: &str = "offline";

/// Payload for a switch or binary sensor that is on.
pub const PAYLOAD_ON: &str = "ON";

/// Payload for a switch or binary sensor that is off.
pub const PAYLOAD_OFF: &str = "OFF";

/// Unit of every temperature, as a JSON escape.
///
/// Home Assistant only accepts the real degree sign for the `temperature`
/// device class, and these payloads are JSON, so the escape is the portable
/// way to send U+00B0 from an ASCII-only source file: the broker and HA see
/// `C` preceded by a degree sign, the firmware image carries seven ASCII
/// bytes.
const UNIT_CELSIUS: &str = "\\u00b0C";

/// Device block shared by every discovery payload.
const DEVICE_NAME: &str = "Pool heat pump";
const DEVICE_MODEL: &str = "WFI-028T";
const DEVICE_MANUFACTURER: &str = "W'Eau";

/// What kind of Home Assistant entity a row describes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// `switch`, `ON`/`OFF` both ways.
    Switch,
    /// `select` over heat / cool / auto.
    Mode,
    /// `number` in whole degrees, with the manual's inclusive range.
    Number {
        /// Lowest value the manual allows.
        min: u8,
        /// Highest value the manual allows.
        max: u8,
    },
    /// `sensor`.
    Sensor {
        /// HA `device_class`, where one fits.
        class: Option<&'static str>,
        /// HA `unit_of_measurement`.
        unit: Option<&'static str>,
        /// HA `state_class`.
        state_class: Option<&'static str>,
    },
    /// `binary_sensor`, `ON`/`OFF`.
    Binary {
        /// HA `device_class`, where one fits.
        class: Option<&'static str>,
    },
}

impl Kind {
    /// The HA component, i.e. the second segment of the discovery topic.
    #[must_use]
    pub const fn component(self) -> &'static str {
        match self {
            Self::Switch => "switch",
            Self::Mode => "select",
            Self::Number { .. } => "number",
            Self::Sensor { .. } => "sensor",
            Self::Binary { .. } => "binary_sensor",
        }
    }

    /// Whether this kind has a command topic.
    #[must_use]
    pub const fn writable(self) -> bool {
        matches!(self, Self::Switch | Self::Mode | Self::Number { .. })
    }
}

/// One Home Assistant entity.
#[derive(Debug, Clone, Copy)]
pub struct Entity {
    /// HA object id - published as `object_id` behind a [`DEVICE_ID`] prefix,
    /// the key in the state document, and the middle segment of the command
    /// topic.
    pub object_id: &'static str,
    /// Human name; HA prefixes the device name.
    pub name: &'static str,
    /// What kind of entity it is.
    pub kind: Kind,
    /// `true` for `entity_category: diagnostic`.
    pub diagnostic: bool,
}

/// Shorthand for a temperature sensor row.
const fn temp(object_id: &'static str, name: &'static str) -> Entity {
    Entity {
        object_id,
        name,
        kind: Kind::Sensor {
            class: Some("temperature"),
            unit: Some(UNIT_CELSIUS),
            state_class: Some("measurement"),
        },
        diagnostic: false,
    }
}

/// Shorthand for a plain measurement row.
const fn measure(
    object_id: &'static str,
    name: &'static str,
    class: Option<&'static str>,
    unit: Option<&'static str>,
) -> Entity {
    Entity {
        object_id,
        name,
        kind: Kind::Sensor {
            class,
            unit,
            state_class: Some("measurement"),
        },
        diagnostic: false,
    }
}

/// Shorthand for a diagnostic counter row.
const fn counter(object_id: &'static str, name: &'static str) -> Entity {
    Entity {
        object_id,
        name,
        kind: Kind::Sensor {
            class: None,
            unit: None,
            state_class: Some("total_increasing"),
        },
        diagnostic: true,
    }
}

/// Shorthand for a binary sensor row.
const fn binary(
    object_id: &'static str,
    name: &'static str,
    class: Option<&'static str>,
    diagnostic: bool,
) -> Entity {
    Entity {
        object_id,
        name,
        kind: Kind::Binary { class },
        diagnostic,
    }
}

/// Every entity the controller publishes, in discovery order.
pub const ENTITIES: &[Entity] = &[
    // --- Controls (register-map.md "Controls") ---
    Entity {
        object_id: "power",
        name: "Power",
        kind: Kind::Switch,
        diagnostic: false,
    },
    Entity {
        object_id: "boost",
        name: "Boost",
        kind: Kind::Switch,
        diagnostic: false,
    },
    Entity {
        object_id: "stop_at_target",
        name: "Stop at target",
        kind: Kind::Switch,
        diagnostic: false,
    },
    Entity {
        object_id: "mode",
        name: "Mode",
        kind: Kind::Mode,
        diagnostic: false,
    },
    Entity {
        object_id: "p01",
        name: "Heating setpoint",
        kind: Kind::Number { min: 8, max: 40 },
        diagnostic: false,
    },
    Entity {
        object_id: "p02",
        name: "Cooling setpoint",
        kind: Kind::Number { min: 8, max: 28 },
        diagnostic: false,
    },
    Entity {
        object_id: "p03",
        name: "Auto setpoint",
        kind: Kind::Number { min: 8, max: 40 },
        diagnostic: false,
    },
    Entity {
        object_id: "p04",
        name: "Restart hysteresis",
        kind: Kind::Number { min: 1, max: 18 },
        diagnostic: false,
    },
    // --- Sensors A01-A14 plus the target frequency ---
    temp("inlet_water", "Inlet water temperature"),
    temp("outlet_water", "Outlet water temperature"),
    temp("ambient", "Ambient temperature"),
    temp("exhaust", "Exhaust temperature"),
    temp("gas_return", "Gas return temperature"),
    temp("outer_piping", "Outer piping temperature"),
    temp("inner_piping", "Inner piping temperature"),
    temp("radiator", "Radiator temperature"),
    measure("eev_steps", "EEV aperture", None, Some("steps")),
    measure(
        "compressor_current",
        "Compressor current",
        Some("current"),
        Some("A"),
    ),
    measure("dc_bus_volts", "DC bus voltage", Some("voltage"), Some("V")),
    measure(
        "compressor_hz",
        "Compressor frequency",
        Some("frequency"),
        Some("Hz"),
    ),
    measure(
        "compressor_target_hz",
        "Compressor target frequency",
        Some("frequency"),
        Some("Hz"),
    ),
    measure("fan_rpm", "Fan speed", None, Some("rpm")),
    measure("fan2_rpm", "Second fan speed", None, Some("rpm")),
    // --- Status and alarms ---
    binary(
        "water_flow_fault",
        "Water flow fault",
        Some("problem"),
        false,
    ),
    binary("boost_active", "Boost active", None, false),
    binary("water_pump", "Water pump", None, false),
    binary("heating_active", "Heating active", None, false),
    binary("run_permitted", "Run permitted", None, false),
    binary(
        "compressor_running",
        "Compressor running",
        Some("running"),
        false,
    ),
    binary("link", "Modbus link", Some("connectivity"), true),
    // --- Diagnostics ---
    Entity {
        object_id: "controller_mode",
        name: "Controller mode",
        kind: Kind::Sensor {
            class: None,
            unit: None,
            state_class: None,
        },
        diagnostic: true,
    },
    Entity {
        object_id: "last_command",
        name: "Last command",
        kind: Kind::Sensor {
            class: None,
            unit: None,
            state_class: None,
        },
        diagnostic: true,
    },
    counter("requests", "Bus requests"),
    counter("timeouts", "Bus timeouts"),
    counter("writes", "Block writes"),
    counter("write_failures", "Failed commands"),
];

/// Write the retained discovery topic of one entity:
/// `homeassistant/<component>/wfi028t/<object_id>/config`.
///
/// # Errors
///
/// Propagates the writer's error, i.e. a buffer that is too small.
pub fn write_discovery_topic(out: &mut dyn core::fmt::Write, entity: &Entity) -> core::fmt::Result {
    write!(
        out,
        "homeassistant/{}/{DEVICE_ID}/{}/config",
        entity.kind.component(),
        entity.object_id
    )
}

/// Write the command topic of one entity: `wfi028t/<object_id>/set`.
///
/// # Errors
///
/// Propagates the writer's error.
pub fn write_command_topic(out: &mut dyn core::fmt::Write, entity: &Entity) -> core::fmt::Result {
    write!(out, "{DEVICE_ID}/{}/set", entity.object_id)
}

/// Write the retained discovery payload of one entity.
///
/// Every payload carries the same `device` block (so HA groups all 30
/// entities under one device), the shared availability topic, and a
/// `value_template` that pulls this entity's key out of the one state
/// document. Writable entities also get a command topic; they are **not**
/// optimistic, because a `state_topic` is always present - HA therefore only
/// shows a new value once the heat pump has reported it back.
///
/// `unique_id` and `object_id` are both `wfi028t_<object_id>` and do
/// different jobs: the first is the identity HA keys its entity registry on,
/// the second is what it builds the entity id from. Home Assistant reads
/// `object_id` only when it first registers an entity, so republishing this
/// payload never renames an entity that already exists - an id assigned
/// before this field was sent has to be changed in HA, or the device removed
/// from the MQTT integration and rediscovered.
///
/// # Errors
///
/// Propagates the writer's error.
pub fn write_discovery(
    out: &mut dyn core::fmt::Write,
    entity: &Entity,
    version: &str,
) -> core::fmt::Result {
    write!(
        out,
        "{{\"name\":\"{}\",\"unique_id\":\"{DEVICE_ID}_{}\",\
         \"object_id\":\"{DEVICE_ID}_{}\",\
         \"state_topic\":\"{TOPIC_STATE}\",\
         \"value_template\":\"{{{{ value_json.{} }}}}\",\
         \"availability_topic\":\"{TOPIC_AVAILABILITY}\",\
         \"payload_available\":\"{PAYLOAD_ONLINE}\",\
         \"payload_not_available\":\"{PAYLOAD_OFFLINE}\"",
        entity.name, entity.object_id, entity.object_id, entity.object_id
    )?;

    if entity.kind.writable() {
        out.write_str(",\"command_topic\":\"")?;
        write_command_topic(out, entity)?;
        out.write_str("\"")?;
    }

    match entity.kind {
        Kind::Switch => write!(
            out,
            ",\"payload_on\":\"{PAYLOAD_ON}\",\"payload_off\":\"{PAYLOAD_OFF}\",\
             \"state_on\":\"{PAYLOAD_ON}\",\"state_off\":\"{PAYLOAD_OFF}\""
        )?,
        Kind::Mode => out.write_str(",\"options\":[\"heat\",\"cool\",\"auto\"]")?,
        Kind::Number { min, max } => write!(
            out,
            ",\"min\":{min},\"max\":{max},\"step\":1,\"mode\":\"box\",\
             \"unit_of_measurement\":\"{UNIT_CELSIUS}\""
        )?,
        Kind::Sensor {
            class,
            unit,
            state_class,
        } => {
            if let Some(class) = class {
                write!(out, ",\"device_class\":\"{class}\"")?;
            }
            if let Some(unit) = unit {
                write!(out, ",\"unit_of_measurement\":\"{unit}\"")?;
            }
            if let Some(state_class) = state_class {
                write!(out, ",\"state_class\":\"{state_class}\"")?;
            }
        }
        Kind::Binary { class } => {
            write!(
                out,
                ",\"payload_on\":\"{PAYLOAD_ON}\",\"payload_off\":\"{PAYLOAD_OFF}\""
            )?;
            if let Some(class) = class {
                write!(out, ",\"device_class\":\"{class}\"")?;
            }
        }
    }

    if entity.diagnostic {
        out.write_str(",\"entity_category\":\"diagnostic\"")?;
    }

    write!(
        out,
        ",\"device\":{{\"identifiers\":[\"{DEVICE_ID}\"],\"name\":\"{DEVICE_NAME}\",\
         \"model\":\"{DEVICE_MODEL}\",\"manufacturer\":\"{DEVICE_MANUFACTURER}\",\
         \"sw_version\":\"{version}\"}}}}"
    )
}

/// The `<entity>` part of a `wfi028t/<entity>/set` topic.
#[must_use]
pub fn command_object(topic: &str) -> Option<&str> {
    let rest = topic.strip_prefix(DEVICE_ID)?.strip_prefix('/')?;
    let object = rest.strip_suffix("/set")?;
    if object.is_empty() || object.contains('/') {
        None
    } else {
        Some(object)
    }
}

/// Turn one command payload into a validated [`Command`].
///
/// Payloads are the ones the discovery config declares and nothing else:
/// `ON`/`OFF` for switches (case-insensitive), the three mode names, and a
/// whole number for the setpoints - with a `.0`-style fraction tolerated,
/// because Home Assistant's number entity may send `34.0` for a step of 1.
/// Range checking is `hp_model`'s job and happens when the command reaches
/// the bus master.
///
/// # Errors
///
/// A short reason, meant for the capture stream, for an unknown entity or a
/// payload that is not one of the above.
pub fn parse_set(object: &str, payload: &str) -> Result<Command, &'static str> {
    let entity = ENTITIES
        .iter()
        .find(|entity| entity.object_id == object)
        .ok_or("unknown entity")?;
    if !entity.kind.writable() {
        return Err("entity is read-only");
    }

    match entity.object_id {
        "power" => on_off(payload).map(Command::SetPower),
        "boost" => on_off(payload).map(Command::SetBoost),
        "stop_at_target" => on_off(payload).map(Command::SetStopAtTarget),
        "mode" => mode(payload).map(Command::SetMode),
        "p01" => degrees(payload).map(Command::SetHeatSetpoint),
        "p02" => degrees(payload).map(Command::SetCoolSetpoint),
        "p03" => degrees(payload).map(Command::SetAutoSetpoint),
        "p04" => degrees(payload).map(Command::SetHysteresis),
        // Unreachable while every writable row above is handled; a new
        // writable row without a parser lands here instead of silently
        // becoming a wrong command.
        _ => Err("no parser for this entity"),
    }
}

fn on_off(payload: &str) -> Result<bool, &'static str> {
    if payload.eq_ignore_ascii_case(PAYLOAD_ON) {
        Ok(true)
    } else if payload.eq_ignore_ascii_case(PAYLOAD_OFF) {
        Ok(false)
    } else {
        Err("expected ON or OFF")
    }
}

fn mode(payload: &str) -> Result<Mode, &'static str> {
    if payload.eq_ignore_ascii_case("heat") {
        Ok(Mode::Heat)
    } else if payload.eq_ignore_ascii_case("cool") {
        Ok(Mode::Cool)
    } else if payload.eq_ignore_ascii_case("auto") {
        Ok(Mode::Auto)
    } else {
        Err("expected heat, cool or auto")
    }
}

fn degrees(payload: &str) -> Result<u8, &'static str> {
    const BAD: &str = "expected a whole number of degrees";
    let (whole, fraction) = match payload.split_once('.') {
        Some((whole, fraction)) => (whole, fraction),
        None => (payload, ""),
    };
    if !fraction.is_empty() && !fraction.bytes().all(|b| b == b'0') {
        return Err("fractional degrees are not settable");
    }
    whole.parse::<u8>().map_err(|_| BAD)
}

#[cfg(test)]
mod tests {
    use super::{
        command_object, parse_set, write_command_topic, write_discovery, write_discovery_topic,
        Entity, Kind, ENTITIES,
    };
    use hp_model::settings::Mode;
    use hp_model::Command;

    type Text = std::string::String;

    fn discovery_of(object_id: &str) -> Text {
        let entity = find(object_id);
        let mut out = Text::new();
        write_discovery(&mut out, entity, "0.1.0").unwrap();
        out
    }

    fn find(object_id: &str) -> &'static Entity {
        ENTITIES
            .iter()
            .find(|e| e.object_id == object_id)
            .expect("entity exists")
    }

    #[test]
    fn object_ids_are_unique_and_topic_safe() {
        for (i, entity) in ENTITIES.iter().enumerate() {
            assert!(
                !entity.object_id.is_empty()
                    && entity
                        .object_id
                        .bytes()
                        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_'),
                "{}",
                entity.object_id
            );
            for other in &ENTITIES[i + 1..] {
                assert_ne!(entity.object_id, other.object_id);
            }
        }
        // The register map's Target section, counted: 8 controls, 15 sensors,
        // 7 status bits, 6 diagnostics.
        assert_eq!(ENTITIES.len(), 36);
        assert_eq!(ENTITIES.iter().filter(|e| e.kind.writable()).count(), 8);
    }

    #[test]
    fn topics_follow_the_documented_layout() {
        let mut topic = Text::new();
        write_discovery_topic(&mut topic, find("power")).unwrap();
        assert_eq!(topic, "homeassistant/switch/wfi028t/power/config");
        let mut topic = Text::new();
        write_discovery_topic(&mut topic, find("mode")).unwrap();
        assert_eq!(topic, "homeassistant/select/wfi028t/mode/config");
        let mut topic = Text::new();
        write_discovery_topic(&mut topic, find("inlet_water")).unwrap();
        assert_eq!(topic, "homeassistant/sensor/wfi028t/inlet_water/config");
        let mut topic = Text::new();
        write_discovery_topic(&mut topic, find("link")).unwrap();
        assert_eq!(topic, "homeassistant/binary_sensor/wfi028t/link/config");
        let mut topic = Text::new();
        write_command_topic(&mut topic, find("p01")).unwrap();
        assert_eq!(topic, "wfi028t/p01/set");
    }

    #[test]
    fn every_discovery_payload_is_balanced_json_with_the_device_block() {
        for entity in ENTITIES {
            let mut out = Text::new();
            write_discovery(&mut out, entity, "0.1.0").unwrap();
            assert!(out.starts_with('{') && out.ends_with('}'), "{out}");
            let opens = out.bytes().filter(|&b| b == b'{').count();
            let closes = out.bytes().filter(|&b| b == b'}').count();
            assert_eq!(opens, closes, "{out}");
            // Quotes come in pairs: no stray quote from a name or unit.
            assert_eq!(out.bytes().filter(|&b| b == b'"').count() % 2, 0, "{out}");
            assert!(out.contains("\"identifiers\":[\"wfi028t\"]"), "{out}");
            assert!(out.contains("\"sw_version\":\"0.1.0\""), "{out}");
            // Registry identity and entity id are separate fields with the
            // same value: both follow the register name, never `name`.
            assert!(
                out.contains(&std::format!(
                    "\"unique_id\":\"wfi028t_{}\"",
                    entity.object_id
                )),
                "{out}"
            );
            assert!(
                out.contains(&std::format!(
                    "\"object_id\":\"wfi028t_{}\"",
                    entity.object_id
                )),
                "{out}"
            );
            assert!(out.contains("\"availability_topic\":\"wfi028t/availability\""));
            assert!(out.contains("\"state_topic\":\"wfi028t/state\""));
            assert!(
                out.contains(&std::format!("value_json.{}", entity.object_id)),
                "{out}"
            );
            assert_eq!(
                out.contains("\"command_topic\""),
                entity.kind.writable(),
                "{out}"
            );
            assert_eq!(
                out.contains("\"entity_category\":\"diagnostic\""),
                entity.diagnostic,
                "{out}"
            );
            // The payload has to fit the buffer the task reserves for it.
            assert!(out.len() < 640, "{} bytes: {out}", out.len());
        }
    }

    #[test]
    fn control_payloads_say_what_home_assistant_needs() {
        let power = discovery_of("power");
        assert!(power.contains("\"payload_on\":\"ON\""));
        assert!(power.contains("\"command_topic\":\"wfi028t/power/set\""));
        // A state topic is present, so HA is not optimistic about a switch.
        assert!(power.contains("\"state_topic\""));

        assert!(discovery_of("mode").contains("\"options\":[\"heat\",\"cool\",\"auto\"]"));

        let p02 = discovery_of("p02");
        assert!(p02.contains("\"min\":8,\"max\":28,\"step\":1,\"mode\":\"box\""));
        assert!(p02.contains("\"unit_of_measurement\":\"\\u00b0C\""));
        let p04 = discovery_of("p04");
        assert!(p04.contains("\"min\":1,\"max\":18"));

        let inlet = discovery_of("inlet_water");
        assert!(inlet.contains("\"device_class\":\"temperature\""));
        assert!(inlet.contains("\"unit_of_measurement\":\"\\u00b0C\""));
        assert!(inlet.contains("\"state_class\":\"measurement\""));
        let current = discovery_of("compressor_current");
        assert!(current.contains("\"device_class\":\"current\""));
        assert!(current.contains("\"unit_of_measurement\":\"A\""));
        assert!(discovery_of("dc_bus_volts").contains("\"device_class\":\"voltage\""));
        assert!(discovery_of("compressor_hz").contains("\"device_class\":\"frequency\""));
        // No device class invented for the fan or the EEV.
        assert!(!discovery_of("fan_rpm").contains("device_class"));
        assert!(!discovery_of("eev_steps").contains("device_class"));

        assert!(discovery_of("water_flow_fault").contains("\"device_class\":\"problem\""));
        let link = discovery_of("link");
        assert!(link.contains("\"device_class\":\"connectivity\""));
        assert!(link.contains("\"entity_category\":\"diagnostic\""));
        assert!(discovery_of("requests").contains("\"state_class\":\"total_increasing\""));
    }

    #[test]
    fn entity_ids_are_pinned_to_the_register_name() {
        // Home Assistant derives the entity id from `object_id`, so these are
        // the ids docs/home-assistant-dashboard.md refers to. Without the
        // field HA would slugify the device name plus `name` instead, and
        // every rename below would break a dashboard.
        for (object_id, entity_id) in [
            ("p01", "number.wfi028t_p01"),
            ("mode", "select.wfi028t_mode"),
            ("power", "switch.wfi028t_power"),
            ("inlet_water", "sensor.wfi028t_inlet_water"),
            ("link", "binary_sensor.wfi028t_link"),
        ] {
            let entity = find(object_id);
            let payload = discovery_of(object_id);
            let expected = std::format!("\"object_id\":\"wfi028t_{object_id}\"");
            assert!(payload.contains(&expected), "{payload}");
            // The documented id is exactly component + the published id.
            assert_eq!(
                std::format!("{}.wfi028t_{object_id}", entity.kind.component()),
                entity_id
            );
        }
    }

    #[test]
    fn command_topics_resolve_to_entities() {
        assert_eq!(command_object("wfi028t/power/set"), Some("power"));
        assert_eq!(command_object("wfi028t/p01/set"), Some("p01"));
        assert_eq!(command_object("wfi028t/state"), None);
        assert_eq!(command_object("wfi028t//set"), None);
        assert_eq!(command_object("wfi028t/a/b/set"), None);
        assert_eq!(command_object("other/power/set"), None);
        assert_eq!(command_object("wfi028t/power/set/x"), None);
    }

    #[test]
    fn payloads_parse_strictly() {
        assert_eq!(parse_set("power", "ON"), Ok(Command::SetPower(true)));
        assert_eq!(parse_set("power", "off"), Ok(Command::SetPower(false)));
        assert_eq!(parse_set("boost", "ON"), Ok(Command::SetBoost(true)));
        assert_eq!(
            parse_set("stop_at_target", "OFF"),
            Ok(Command::SetStopAtTarget(false))
        );
        assert_eq!(parse_set("mode", "cool"), Ok(Command::SetMode(Mode::Cool)));
        assert_eq!(parse_set("mode", "AUTO"), Ok(Command::SetMode(Mode::Auto)));
        assert_eq!(parse_set("p01", "34"), Ok(Command::SetHeatSetpoint(34)));
        // HA's number entity may spell a step of 1 as "34.0".
        assert_eq!(parse_set("p01", "34.0"), Ok(Command::SetHeatSetpoint(34)));
        assert_eq!(parse_set("p04", "2"), Ok(Command::SetHysteresis(2)));
        // Out of range is not this layer's call: hp_model rejects it, which is
        // what gets reported back as an outcome.
        assert_eq!(parse_set("p02", "99"), Ok(Command::SetCoolSetpoint(99)));

        assert!(parse_set("power", "1").is_err());
        assert!(parse_set("power", "").is_err());
        assert!(parse_set("power", "ON ").is_err());
        assert!(parse_set("mode", "dry").is_err());
        assert!(parse_set("p01", "34.5").is_err());
        assert!(parse_set("p01", "-1").is_err());
        assert!(parse_set("p01", "300").is_err());
        assert!(parse_set("p01", "").is_err());
        assert!(parse_set("inlet_water", "20").is_err());
        assert!(parse_set("nonsense", "ON").is_err());
    }

    #[test]
    fn writable_kinds_are_exactly_the_controls() {
        for entity in ENTITIES {
            let writable = matches!(entity.kind, Kind::Switch | Kind::Mode | Kind::Number { .. });
            assert_eq!(entity.kind.writable(), writable);
            // Every writable entity has a working parser.
            if writable {
                assert!(
                    parse_set(entity.object_id, "ON").is_ok()
                        || parse_set(entity.object_id, "heat").is_ok()
                        || parse_set(entity.object_id, "10").is_ok(),
                    "{}",
                    entity.object_id
                );
            }
        }
    }
}
