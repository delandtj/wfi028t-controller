//! The one retained state document, built straight into a caller-owned
//! buffer. Pure: no I/O, no clock, no allocation.
//!
//! Every key here is the `object_id` of an entity in [`super::entity`], and
//! every entity's `value_template` reads exactly one of these keys. A value
//! the heat pump does not report (`0x7fff`, or no block decoded yet) is
//! `null`: Home Assistant's MQTT platforms render a Jinja `None` as the
//! string `None` and turn that into "unknown", which is the honest state -
//! as opposed to `0`, which would look like a real reading.
//!
//! # Host tests
//!
//! The firmware crate only builds for `riscv32imac-unknown-none-elf`, so
//! `cargo test -p wfi-controller-fw` cannot run (a `no_std` binary has no
//! `test` crate). The tests in this module and in [`super::entity`] and
//! [`super::config`] are pure and were run on the host through a throwaway
//! harness crate that includes the three files by path:
//!
//! ```text
//! # Cargo.toml: heapless = "0.8", hp-model = { path = ".../hp-model" }
//! #[path = ".../fw/src/mqtt/entity.rs"] pub mod entity;
//! #[path = ".../fw/src/mqtt/json.rs"]   pub mod json;
//! #[path = ".../fw/src/mqtt/config.rs"] pub mod config;
//! ```
//!
//! `cargo test` in that harness runs them unchanged (21 tests). Nothing in
//! these three modules may therefore refer to `crate::` or to anything
//! hardware-bound - which is also what keeps them reviewable on their own.

use core::fmt::Write;

use hp_model::settings::Mode;
use hp_model::{DeciCelsius, Settings, Status};

/// The bus counters that reach Home Assistant. A subset of the firmware's
/// [`crate::master::Counters`]: the four that say something an operator acts
/// on. The rest stay in the `status` line.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Counters {
    /// Requests sent.
    pub requests: u32,
    /// Requests that went unanswered.
    pub timeouts: u32,
    /// Whole-block writes sent.
    pub writes: u32,
    /// Commands that did not end in `Applied`.
    pub write_failures: u32,
}

/// Everything the state document is built from: the decoded blocks plus the
/// controller's own state.
#[derive(Debug, Clone, Copy)]
pub struct View<'a> {
    /// Last decoded status block, if any.
    pub status: Option<&'a Status>,
    /// Last decoded settings block, if any.
    pub settings: Option<&'a Settings>,
    /// Whether the heat pump is answering.
    pub link_up: bool,
    /// `"listen"` or `"master"`.
    pub controller_mode: &'static str,
    /// The last command and its outcome, as the line interface spells it.
    pub last_command: &'a str,
    /// Bus counters since boot.
    pub counters: Counters,
}

/// First key of the counter block, which is also where the document stops
/// being worth comparing: see [`volatile_free`].
const COUNTERS_AT: &str = ",\"requests\":";

/// The part of a state document that is not a monotonic counter.
///
/// [`Counters::requests`] grows on every single poll, so a byte-for-byte
/// comparison of two documents would report a change twice a second and
/// publish the whole thing each time. The counters are written last and
/// nothing else in the document only ever grows, so comparing the part
/// before them is what "the state changed" actually means. The counters then
/// ride along with whatever publication the next real change (or the 60 s
/// refresh) produces.
#[must_use]
pub fn volatile_free(document: &str) -> &str {
    match document.find(COUNTERS_AT) {
        Some(at) => &document[..at],
        None => document,
    }
}

/// Write the whole state document.
///
/// # Errors
///
/// Propagates the writer's error, i.e. a buffer that is too small. A
/// half-written document is never published: the caller throws the buffer
/// away.
pub fn write_state(out: &mut dyn Write, view: &View<'_>) -> core::fmt::Result {
    let mut doc = Doc { out, first: true };

    // --- Controls, from the settings block ---
    match view.settings {
        Some(settings) => {
            doc.flag("power", settings.power())?;
            doc.flag("boost", settings.boost())?;
            doc.flag("stop_at_target", settings.stop_at_target())?;
            doc.key("mode")?;
            match settings.mode() {
                Mode::Heat => doc.out.write_str("\"heat\"")?,
                Mode::Cool => doc.out.write_str("\"cool\"")?,
                Mode::Auto => doc.out.write_str("\"auto\"")?,
                // A mode we never confirmed. Not one of the select's options,
                // so it is reported as unknown rather than as a wrong option.
                Mode::Other(_) => doc.out.write_str("null")?,
            }
            doc.number("p01", Some(settings.heat_setpoint()))?;
            doc.number("p02", Some(settings.cool_setpoint()))?;
            doc.number("p03", Some(settings.auto_setpoint()))?;
            doc.number("p04", Some(settings.hysteresis()))?;
        }
        None => doc.all_null(&[
            "power",
            "boost",
            "stop_at_target",
            "mode",
            "p01",
            "p02",
            "p03",
            "p04",
        ])?,
    }

    // --- Sensors and status bits, from the status block ---
    match view.status {
        Some(status) => {
            doc.temp("inlet_water", status.inlet_water())?;
            doc.temp("outlet_water", status.outlet_water())?;
            doc.temp("ambient", status.ambient())?;
            doc.temp("exhaust", status.exhaust())?;
            doc.temp("gas_return", status.gas_return())?;
            doc.temp("outer_piping", status.outer_piping())?;
            doc.temp("inner_piping", status.inner_piping())?;
            doc.temp("radiator", status.radiator())?;
            doc.number("eev_steps", status.eev_steps())?;
            doc.number("compressor_current", status.compressor_current_a())?;
            doc.number("dc_bus_volts", status.dc_bus_volts())?;
            doc.number("compressor_hz", status.compressor_hz())?;
            doc.number("compressor_target_hz", status.compressor_target_hz())?;
            doc.number("fan_rpm", status.fan_rpm())?;
            doc.number("fan2_rpm", status.fan2_rpm())?;

            doc.flag("water_flow_fault", status.water_flow_fault())?;
            doc.flag("boost_active", status.boost_active())?;
            doc.flag("water_pump", status.water_pump())?;
            doc.flag("heating_active", status.compressor_output())?;
            doc.flag("run_permitted", status.heating_demand())?;
            doc.flag("compressor_running", status.compressor_running())?;
        }
        None => doc.all_null(&[
            "inlet_water",
            "outlet_water",
            "ambient",
            "exhaust",
            "gas_return",
            "outer_piping",
            "inner_piping",
            "radiator",
            "eev_steps",
            "compressor_current",
            "dc_bus_volts",
            "compressor_hz",
            "compressor_target_hz",
            "fan_rpm",
            "fan2_rpm",
            "water_flow_fault",
            "boost_active",
            "water_pump",
            "heating_active",
            "run_permitted",
            "compressor_running",
        ])?,
    }

    // --- Diagnostics ---
    doc.flag("link", view.link_up)?;
    doc.key("controller_mode")?;
    write!(doc.out, "\"{}\"", view.controller_mode)?;
    doc.key("last_command")?;
    doc.string(view.last_command)?;
    // The counters come last, and [`COUNTERS_AT`] says so.
    doc.number("requests", Some(view.counters.requests))?;
    doc.number("timeouts", Some(view.counters.timeouts))?;
    doc.number("writes", Some(view.counters.writes))?;
    doc.number("write_failures", Some(view.counters.write_failures))?;

    doc.finish()
}

/// A JSON object under construction: tracks only whether a comma is due.
struct Doc<'w> {
    out: &'w mut dyn Write,
    first: bool,
}

impl Doc<'_> {
    /// Open the object or separate from the previous field, then the key.
    fn key(&mut self, name: &str) -> core::fmt::Result {
        self.out.write_char(if core::mem::take(&mut self.first) {
            '{'
        } else {
            ','
        })?;
        write!(self.out, "\"{name}\":")
    }

    fn finish(&mut self) -> core::fmt::Result {
        if self.first {
            self.out.write_char('{')?;
            self.first = false;
        }
        self.out.write_char('}')
    }

    /// An `ON`/`OFF` field, for switches and binary sensors.
    fn flag(&mut self, name: &str, on: bool) -> core::fmt::Result {
        self.key(name)?;
        self.out.write_str(if on { "\"ON\"" } else { "\"OFF\"" })
    }

    /// A numeric field, `null` when the heat pump does not report it.
    fn number<T: core::fmt::Display>(&mut self, name: &str, value: Option<T>) -> core::fmt::Result {
        self.key(name)?;
        match value {
            Some(value) => write!(self.out, "{value}"),
            None => self.out.write_str("null"),
        }
    }

    /// A temperature as a JSON number with one decimal: the model carries
    /// tenths of a degree, and HA wants degrees.
    fn temp(&mut self, name: &str, value: Option<DeciCelsius>) -> core::fmt::Result {
        self.key(name)?;
        match value {
            Some(value) => {
                let tenths = value.tenths();
                let sign = if tenths < 0 { "-" } else { "" };
                let magnitude = tenths.unsigned_abs();
                write!(self.out, "{sign}{}.{}", magnitude / 10, magnitude % 10)
            }
            None => self.out.write_str("null"),
        }
    }

    /// Every one of `names` as `null`: no block decoded yet.
    fn all_null(&mut self, names: &[&str]) -> core::fmt::Result {
        for name in names {
            self.key(name)?;
            self.out.write_str("null")?;
        }
        Ok(())
    }

    /// A JSON string, with the characters that could break the document
    /// escaped. Our own text never contains them; a future caller's might.
    fn string(&mut self, text: &str) -> core::fmt::Result {
        self.out.write_char('"')?;
        for ch in text.chars() {
            match ch {
                '"' => self.out.write_str("\\\"")?,
                '\\' => self.out.write_str("\\\\")?,
                // Control characters are not legal raw in a JSON string.
                c if (c as u32) < 0x20 => write!(self.out, "\\u{:04x}", c as u32)?,
                c => self.out.write_char(c)?,
            }
        }
        self.out.write_char('"')
    }
}

#[cfg(test)]
mod tests {
    use super::{write_state, Counters, View};
    use hp_model::status::NOT_PRESENT;
    use hp_model::{Settings, Status};

    type Text = std::string::String;

    /// The capture snapshot of `docs/register-map.md` (2026-10-03 15:32): unit
    /// on, heating, ECO, P01 33, P02 27, P03 27, P04 1.
    const STATUS_BLOCK: [u16; hp_model::STATUS_LEN] = [
        0x2020, 0x0404, 0x0000, 0x000d, 0x0021, 0x0080, 0x0014, 0x0000, 0x0000, 0x0000, 0x0000,
        0x0000, 0x0000, 0x0000, 0x7fff, 0x0113, 0x003b, 0x0028, 0x000c, 0x000e, 0x0041, 0x004c,
        0x7fff, 0x7fff, 0x0082, 0x7fff, 0x0037, 0x0036, 0x0000, 0x0000, 0x021d, 0x0050, 0x0008,
        0x7fff, 0x7fff, 0x7fff, 0x02d2, 0x0000, 0x0000, 0x0000, 0x0000, 0x0000, 0x7fff, 0x7fff,
        0x0000, 0x7fff, 0x0000, 0x0000, 0x0000, 0x0000, 0x0000, 0x0000, 0x0000, 0x7fff, 0x7fff,
        0x7fff, 0x7fff, 0x7fff, 0x7fff, 0x7fff, 0x7fff, 0x7fff, 0x77ff,
    ];

    const SETTINGS_BLOCK: [u16; hp_model::SETTINGS_LEN] = [
        0x1071, 0x0001, 0x0021, 0x001b, 0x0032, 0x0096, 0xffff, 0xffff, 0x7fff, 0xffff, 0x01f4,
        0x001b, 0x000a, 0xffff, 0x0001, 0x0000, 0xffec, 0x0028, 0xfffa, 0x000b, 0x0010, 0x0006,
        0x0011, 0x001e, 0x0001, 0x0058, 0x0028, 0x0008, 0x0001, 0x0017, 0x0028, 0x002c, 0x0030,
        0x0036, 0x003a, 0x0040, 0x0048, 0x0050, 0x0054, 0x005a, 0x005f, 0x0064, 0x0069, 0x006e,
        0x0073, 0x000c, 0x000d, 0x000e, 0x002e, 0x0034, 0x003a, 0x0040, 0x0048, 0x0055, 0x0000,
        0x0001, 0x000c, 0xffff, 0x0000, 0x0008, 0x0000, 0x000c, 0x0000, 0x000e, 0x0000, 0x0011,
        0x0000,
    ];

    fn full() -> Text {
        let status = Status::from_block(STATUS_BLOCK);
        let settings = Settings::from_block(SETTINGS_BLOCK);
        render(&View {
            status: Some(&status),
            settings: Some(&settings),
            link_up: true,
            controller_mode: "master",
            last_command: "p01 34 -> ok",
            counters: Counters {
                requests: 1234,
                timeouts: 2,
                writes: 3,
                write_failures: 1,
            },
        })
    }

    fn render(view: &View<'_>) -> Text {
        let mut out = Text::new();
        write_state(&mut out, view).unwrap();
        out
    }

    #[test]
    fn document_is_one_flat_json_object() {
        let doc = full();
        assert!(doc.starts_with("{\"power\":"));
        assert!(doc.ends_with('}'));
        // Flat: exactly one pair of braces, and no array.
        assert_eq!(doc.bytes().filter(|&b| b == b'{').count(), 1);
        assert_eq!(doc.bytes().filter(|&b| b == b'}').count(), 1);
        assert!(!doc.contains('['));
        // No empty field and no double comma.
        assert!(!doc.contains(",,") && !doc.contains("{,"));
        assert_eq!(doc.bytes().filter(|&b| b == b'"').count() % 2, 0);
    }

    #[test]
    fn every_entity_has_a_key_and_every_key_an_entity() {
        let doc = full();
        for entity in super::super::entity::ENTITIES {
            assert!(
                doc.contains(&std::format!("\"{}\":", entity.object_id)),
                "no state key for {}",
                entity.object_id
            );
        }
        // Count the keys: one per entity, no extras nobody reads.
        let keys = doc.matches("\":").count();
        assert_eq!(keys, super::super::entity::ENTITIES.len());
    }

    #[test]
    fn values_match_the_capture_snapshot() {
        let doc = full();
        // Settings: 0x003f = 0x1071 -> on, P05 set, ECO (so boost off).
        assert!(doc.contains("\"power\":\"ON\""));
        assert!(doc.contains("\"boost\":\"OFF\""));
        assert!(doc.contains("\"stop_at_target\":\"ON\""));
        assert!(doc.contains("\"mode\":\"heat\""));
        assert!(doc.contains("\"p01\":33"));
        assert!(doc.contains("\"p02\":27"));
        assert!(doc.contains("\"p03\":27"));
        assert!(doc.contains("\"p04\":1"));
        // Sensors, scaled as the A01-A14 readout: 275/10, 59/2, 40/2, 76x1.
        assert!(doc.contains("\"inlet_water\":27.5"));
        assert!(doc.contains("\"outlet_water\":29.5"));
        assert!(doc.contains("\"ambient\":20.0"));
        assert!(doc.contains("\"exhaust\":76.0"));
        assert!(doc.contains("\"gas_return\":7.0"));
        assert!(doc.contains("\"outer_piping\":6.0"));
        assert!(doc.contains("\"inner_piping\":32.5"));
        assert!(doc.contains("\"radiator\":40.0"));
        assert!(doc.contains("\"eev_steps\":130"));
        assert!(doc.contains("\"compressor_current\":8"));
        assert!(doc.contains("\"dc_bus_volts\":541"));
        assert!(doc.contains("\"compressor_hz\":54"));
        assert!(doc.contains("\"compressor_target_hz\":55"));
        assert!(doc.contains("\"fan_rpm\":722"));
        assert!(doc.contains("\"fan2_rpm\":0"));
        // Status bits: 0x0004 = 0x0021, 0x0005 = 0x0080.
        assert!(doc.contains("\"water_flow_fault\":\"OFF\""));
        assert!(doc.contains("\"boost_active\":\"OFF\""));
        assert!(doc.contains("\"water_pump\":\"ON\""));
        assert!(doc.contains("\"heating_active\":\"ON\""));
        assert!(doc.contains("\"run_permitted\":\"ON\""));
        assert!(doc.contains("\"compressor_running\":\"ON\""));
        // Diagnostics.
        assert!(doc.contains("\"link\":\"ON\""));
        assert!(doc.contains("\"controller_mode\":\"master\""));
        assert!(doc.contains("\"last_command\":\"p01 34 -> ok\""));
        assert!(doc.contains("\"requests\":1234"));
        assert!(doc.contains("\"write_failures\":1"));
    }

    #[test]
    fn absent_sensors_and_unknown_blocks_are_null() {
        let mut block = STATUS_BLOCK;
        for addr in [0x000fu16, 0x0010, 0x0015, 0x0018, 0x0020, 0x001b, 0x0024] {
            block[addr as usize] = NOT_PRESENT;
        }
        let status = Status::from_block(block);
        let doc = render(&View {
            status: Some(&status),
            settings: None,
            link_up: false,
            controller_mode: "listen",
            last_command: "",
            counters: Counters::default(),
        });
        assert!(doc.contains("\"inlet_water\":null"));
        assert!(doc.contains("\"outlet_water\":null"));
        assert!(doc.contains("\"exhaust\":null"));
        assert!(doc.contains("\"eev_steps\":null"));
        assert!(doc.contains("\"compressor_current\":null"));
        assert!(doc.contains("\"compressor_hz\":null"));
        assert!(doc.contains("\"fan_rpm\":null"));
        // A sensor that is still present keeps its value.
        assert!(doc.contains("\"ambient\":20.0"));
        // Compressor frequency unknown -> not running.
        assert!(doc.contains("\"compressor_running\":\"OFF\""));
        // No settings block yet: every control is unknown, not a made-up zero.
        for key in ["power", "boost", "stop_at_target", "mode", "p01", "p04"] {
            assert!(doc.contains(&std::format!("\"{key}\":null")), "{key}");
        }
        assert!(doc.contains("\"link\":\"OFF\""));
        assert!(doc.contains("\"controller_mode\":\"listen\""));
        assert!(doc.contains("\"last_command\":\"\""));
    }

    #[test]
    fn nothing_decoded_yet_is_all_null() {
        let doc = render(&View {
            status: None,
            settings: None,
            link_up: false,
            controller_mode: "listen",
            last_command: "",
            counters: Counters::default(),
        });
        assert_eq!(doc.matches(":null").count(), 29);
        assert_eq!(
            doc.matches("\":").count(),
            super::super::entity::ENTITIES.len()
        );
    }

    #[test]
    fn negative_temperatures_keep_their_sign_and_decimal() {
        let mut block = STATUS_BLOCK;
        block[0x0011] = (-8i16) as u16; // ambient, raw / 2 -> -4.0
        block[0x000f] = (-25i16) as u16; // inlet, raw / 10 -> -2.5
        block[0x0013] = (-1i16) as u16; // gas return, raw / 2 -> -0.5
        let status = Status::from_block(block);
        let doc = render(&View {
            status: Some(&status),
            settings: None,
            link_up: true,
            controller_mode: "master",
            last_command: "",
            counters: Counters::default(),
        });
        assert!(doc.contains("\"ambient\":-4.0"));
        assert!(doc.contains("\"inlet_water\":-2.5"));
        assert!(doc.contains("\"gas_return\":-0.5"));
    }

    #[test]
    fn strings_are_escaped() {
        let doc = render(&View {
            status: None,
            settings: None,
            link_up: false,
            controller_mode: "listen",
            last_command: "say \"hi\"\\ now\n",
            counters: Counters::default(),
        });
        assert!(doc.contains(r#""last_command":"say \"hi\"\\ now\u000a""#));
        assert_eq!(doc.bytes().filter(|&b| b == b'\n').count(), 0);
    }

    #[test]
    fn counters_do_not_count_as_a_change() {
        use super::volatile_free;
        let status = Status::from_block(STATUS_BLOCK);
        let settings = Settings::from_block(SETTINGS_BLOCK);
        let view = |counters| View {
            status: Some(&status),
            settings: Some(&settings),
            link_up: true,
            controller_mode: "master",
            last_command: "",
            counters,
        };
        let a = render(&view(Counters {
            requests: 10,
            timeouts: 0,
            writes: 0,
            write_failures: 0,
        }));
        let b = render(&view(Counters {
            requests: 11,
            timeouts: 1,
            writes: 2,
            write_failures: 3,
        }));
        assert_ne!(a, b);
        assert_eq!(volatile_free(&a), volatile_free(&b));
        // The counters really are at the end, and the prefix is most of it.
        assert!(volatile_free(&a).len() > a.len() - 80);
        assert!(volatile_free(&a).ends_with("\"last_command\":\"\""));
        // A real change does show up.
        let mut block = STATUS_BLOCK;
        block[0x0010] = 61;
        let warmer = Status::from_block(block);
        let mut changed = view(Counters::default());
        changed.status = Some(&warmer);
        assert_ne!(volatile_free(&a), volatile_free(&render(&changed)));
        // A document without counters (there is none today) is its own key.
        assert_eq!(volatile_free("{\"power\":\"ON\"}"), "{\"power\":\"ON\"}");
    }

    #[test]
    fn the_document_fits_the_task_buffer() {
        // The task publishes out of a fixed buffer; this is the size it has to
        // be chosen against.
        assert!(full().len() < 900, "{} bytes", full().len());
    }
}
