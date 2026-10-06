# Home Assistant dashboard

A dashboard built from the entities `fw/src/mqtt/entity.rs` publishes. It is
deliberately HA-side work: the firmware ships no `climate` entity, for the
reasons in that module's header and in ADR 0001 (component 3).

- A single MQTT `climate` entity carries one `min_temp`/`max_temp` pair, while
  this machine has three setpoints over two ranges (P01/P03 8-40, P02 8-28).
  Discovery cannot vary a range per mode, so a thermostat dial would offer
  35 C in cooling, `hp_model` would reject the write, and the card would
  silently snap back.
- `off` here is a power bit, not a mode. HA's `mode: off` would have to become
  a power write plus a mode write - two commands, one per 1 s cycle, two
  outcomes - over a machine that does not work that way.

The `conditional` cards below get the one-dial behaviour anyway, and each
`number` keeps its own true range, so the UI can never offer an illegal value.

## Entity ids

Every payload carries `object_id`, so ids are `<component>.wfi028t_<key>`
where `<key>` is `Entity::object_id` - the register name, not the display
name. Renaming an `Entity::name` does not move an entity.

| Entity id | What |
|---|---|
| `switch.wfi028t_power` | Power |
| `switch.wfi028t_boost` | Boost |
| `switch.wfi028t_stop_at_target` | Stop at target |
| `select.wfi028t_mode` | Mode: `heat`, `cool`, `auto` |
| `number.wfi028t_p01` | Heating setpoint, 8-40 |
| `number.wfi028t_p02` | Cooling setpoint, 8-28 |
| `number.wfi028t_p03` | Auto setpoint, 8-40 |
| `number.wfi028t_p04` | Restart hysteresis, 1-18 |
| `sensor.wfi028t_inlet_water`, `..._outlet_water`, `..._ambient` | Temperatures |
| `sensor.wfi028t_exhaust`, `..._gas_return` | Refrigerant temperatures |
| `sensor.wfi028t_outer_piping`, `..._inner_piping`, `..._radiator` | Coil temperatures |
| `sensor.wfi028t_compressor_hz`, `..._compressor_target_hz` | Frequency, actual and target |
| `sensor.wfi028t_compressor_current`, `..._dc_bus_volts` | Compressor electrical |
| `sensor.wfi028t_eev_steps`, `..._fan_rpm`, `..._fan2_rpm` | Actuators |
| `binary_sensor.wfi028t_compressor_running`, `..._water_pump`, `..._heating_active` | Running state |
| `binary_sensor.wfi028t_run_permitted`, `..._boost_active` | Permissives |
| `binary_sensor.wfi028t_water_flow_fault` | Alarm |
| `binary_sensor.wfi028t_link` | Modbus link, diagnostic |
| `sensor.wfi028t_controller_mode`, `..._last_command` | Controller diagnostics |
| `sensor.wfi028t_requests`, `..._timeouts`, `..._writes`, `..._write_failures` | Bus counters |

Entities whose ids predate the `object_id` field keep the id they were given:
HA reads `object_id` only when it first registers an entity. To adopt the
table above on an existing install, delete the device under Settings >
Devices & Services > MQTT and let the next discovery burst recreate it, or
rename the ids by hand.

Everything goes `unavailable` when `wfi028t/availability` reads `offline`.
That is the controller's own link to the broker, not the Modbus link - watch
`binary_sensor.wfi028t_link` for the latter, which is why it sits at the top
of the control card below.

## The dashboard

Settings > Dashboards > (your dashboard) > pencil > three dots > Raw
configuration editor, and add this view. Individual cards also paste into
Add card > Manual.

```yaml
views:
  - title: Pool heat pump
    path: heat-pump
    icon: mdi:hot-tub
    type: sections
    max_columns: 3
    sections:
      # --- Control ---------------------------------------------------------
      - type: grid
        cards:
          - type: heading
            heading: Control
            heading_style: title

          - type: conditional
            conditions:
              - condition: state
                entity: binary_sensor.wfi028t_link
                state_not: "on"
            card:
              type: markdown
              content: >-
                **No Modbus link.** Values below are the last ones read and
                commands will not reach the heat pump.

          - type: tile
            entity: switch.wfi028t_power
            name: Power
            grid_options:
              columns: 12
            features:
              - type: toggle

          - type: tile
            entity: select.wfi028t_mode
            name: Mode
            grid_options:
              columns: 12
            features:
              - type: select-options

          # One setpoint control, always the register the current mode uses.
          - type: conditional
            conditions:
              - condition: state
                entity: select.wfi028t_mode
                state: heat
            grid_options:
              columns: 12
            card:
              type: tile
              entity: number.wfi028t_p01
              name: Setpoint (heating)
              features:
                - type: numeric-input
                  style: buttons

          - type: conditional
            conditions:
              - condition: state
                entity: select.wfi028t_mode
                state: cool
            grid_options:
              columns: 12
            card:
              type: tile
              entity: number.wfi028t_p02
              name: Setpoint (cooling)
              features:
                - type: numeric-input
                  style: buttons

          - type: conditional
            conditions:
              - condition: state
                entity: select.wfi028t_mode
                state: auto
            grid_options:
              columns: 12
            card:
              type: tile
              entity: number.wfi028t_p03
              name: Setpoint (auto)
              features:
                - type: numeric-input
                  style: buttons

          - type: tile
            entity: switch.wfi028t_boost
            name: Boost
            features:
              - type: toggle

          - type: tile
            entity: switch.wfi028t_stop_at_target
            name: Stop at target
            features:
              - type: toggle

      # --- At a glance -----------------------------------------------------
      - type: grid
        cards:
          - type: heading
            heading: At a glance
            heading_style: title

          - type: tile
            entity: sensor.wfi028t_inlet_water
            name: Water in
          - type: tile
            entity: sensor.wfi028t_outlet_water
            name: Water out
          - type: tile
            entity: sensor.wfi028t_ambient
            name: Air
          - type: tile
            entity: binary_sensor.wfi028t_compressor_running
            name: Compressor
          - type: tile
            entity: sensor.wfi028t_compressor_hz
            name: Frequency
          - type: tile
            entity: sensor.wfi028t_compressor_current
            name: Current
          - type: tile
            entity: binary_sensor.wfi028t_water_pump
            name: Pump
          - type: tile
            entity: binary_sensor.wfi028t_heating_active
            name: Heating
          - type: tile
            entity: binary_sensor.wfi028t_water_flow_fault
            name: Flow fault

      # --- Trends ----------------------------------------------------------
      - type: grid
        cards:
          - type: heading
            heading: Trends
            heading_style: title

          - type: history-graph
            title: Water and air
            hours_to_show: 24
            entities:
              - entity: sensor.wfi028t_inlet_water
                name: In
              - entity: sensor.wfi028t_outlet_water
                name: Out
              - entity: sensor.wfi028t_ambient
                name: Air

          - type: history-graph
            title: Compressor
            hours_to_show: 24
            entities:
              - entity: sensor.wfi028t_compressor_hz
                name: Actual
              - entity: sensor.wfi028t_compressor_target_hz
                name: Target
              - entity: sensor.wfi028t_compressor_current
                name: Current

      # --- Refrigerant circuit --------------------------------------------
      - type: grid
        cards:
          - type: heading
            heading: Circuit
            heading_style: title

          - type: entities
            entities:
              - entity: sensor.wfi028t_exhaust
                name: Exhaust
              - entity: sensor.wfi028t_gas_return
                name: Gas return
              - entity: sensor.wfi028t_outer_piping
                name: Outer piping
              - entity: sensor.wfi028t_inner_piping
                name: Inner piping
              - entity: sensor.wfi028t_radiator
                name: Radiator
              - type: divider
              - entity: sensor.wfi028t_eev_steps
                name: EEV aperture
              - entity: sensor.wfi028t_dc_bus_volts
                name: DC bus
              - entity: sensor.wfi028t_fan_rpm
                name: Fan
              - entity: sensor.wfi028t_fan2_rpm
                name: Second fan
              - entity: binary_sensor.wfi028t_run_permitted
                name: Heating demand
              - entity: binary_sensor.wfi028t_boost_active
                name: Boost active
              - entity: number.wfi028t_p04
                name: Restart hysteresis

      # --- Diagnostics -----------------------------------------------------
      - type: grid
        cards:
          - type: heading
            heading: Controller
            heading_style: title

          - type: entities
            entities:
              - entity: binary_sensor.wfi028t_link
                name: Modbus link
              - entity: sensor.wfi028t_controller_mode
                name: Controller mode
              - entity: sensor.wfi028t_last_command
                name: Last command
              - type: divider
              - entity: sensor.wfi028t_requests
                name: Bus requests
              - entity: sensor.wfi028t_timeouts
                name: Bus timeouts
              - entity: sensor.wfi028t_writes
                name: Block writes
              - entity: sensor.wfi028t_write_failures
                name: Failed commands
```

## Layout

The view is a **sections** view, so each section is a column and sections sit
next to one another; `max_columns: 3` is the ceiling. Home Assistant drops to
fewer columns as the window narrows, and to one on a phone - a view that
stacks everything on a wide screen is usually a masonry view, which flows
cards into columns by height instead of letting you place them.

Within a section the grid is 12 units wide. A `tile` takes 6 by default, so
the glance tiles pair up two per row; the control rows carry
`grid_options: {columns: 12}` to stay full width, and `entities` and
`history-graph` cards are full width already.

To force two specific cards side by side regardless of view type, a
`horizontal-stack` does it - but it splits the width evenly and does not
reflow on a phone, so it is the wrong tool for a whole dashboard.

## Notes

- `numeric-input` with `style: buttons` steps by the `step` in discovery,
  which is 1 for every setpoint. `style: slider` is the alternative; buttons
  are steadier on a phone.
- The mode conditionals match the payloads in `Kind::Mode`'s `options`
  (`heat`, `cool`, `auto`). Add a mode there and a card here, or the setpoint
  row disappears in that mode.
- Inlet minus outlet is the number that actually tells you whether the pump is
  doing anything. It needs a template sensor (HA-side), so it is not in the
  table above; the two history graphs show the same thing less directly.
- The bus counters are `total_increasing`, so a statistics card over
  `sensor.wfi028t_timeouts` is the honest way to watch bus health over days
  rather than reading the raw total.
