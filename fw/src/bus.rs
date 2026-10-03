//! UART1 as the RS-485 port: the sniffer's t3.5 receiver plus a transmitter.
//!
//! One owner for the whole peripheral, because listening and transmitting are
//! the same half-duplex wire. The sniffer firmware's receiver is kept
//! byte-for-byte in behaviour (FIFO threshold 1, software t3.5 gap, partial
//! frame flushed on a UART error); [`BusUart::send`] is the new half.
//!
//! Two hardware facts shape this module:
//!
//! - The RS-485 module switches direction from the TX line itself, so there is
//!   no DE/RE pin to drive: writing to UART1 TX (GPIO5) is all it takes.
//! - That module may or may not echo our own transmission back into RX. Both
//!   cases have to work, so every frame we send is remembered in `last_tx` and
//!   [`BusUart::take_echo`] recognises it when it comes back. The echo is
//!   recognised at most once per transmission, so a foreign master sending a
//!   frame identical to ours is still seen as foreign from the second frame on.
//!   A *prefix* of what we sent also counts as our echo: a 143-byte settings
//!   write overruns the 128-byte RX FIFO while we are blocked in `write_async`,
//!   and a truncated echo is the expected result.
//!
//! Nothing here publishes received frames; the caller does, so that an echo is
//! not logged twice (it already went out as a `dir=tx` line when it was sent).

use embassy_futures::select::{select, select3, Either, Either3};
use embassy_time::{Duration, Instant, Timer};
use esp_hal::uart::{Config as UartConfig, DataBits, Parity, RxConfig, StopBits, Uart};
use esp_hal::Async;

use modbus_sniffer_core as sniffer;
use sniffer::BusConfig;

/// Bus data into the MCU: the RS-485 module's RXD output (GPIO4 on the
/// DevKitC-1 header).
pub type RxPin = esp_hal::peripherals::GPIO4<'static>;

/// Bus data out of the MCU: the RS-485 module's TXD input (GPIO5). The module
/// derives the direction from this line, so this is the only wiring change
/// against the sniffer.
pub type TxPin = esp_hal::peripherals::GPIO5<'static>;

/// RX FIFO level that wakes `read_async`. Must be 1: the frame timer below
/// restarts every time a read returns, so a read has to return as soon as any
/// byte arrives. With a higher threshold the read stays pending mid-frame, the
/// t3.5 timer fires while bytes are still streaming in, and frames get split
/// (seen on hardware with 32). The 128-byte hardware FIFO still absorbs any
/// latency in servicing the interrupt.
const RX_FIFO_THRESHOLD: u16 = 1;

/// Hardware RX idle timeout, in symbol (character) times. With a FIFO
/// threshold of 1 this rarely matters; it only flushes stragglers. The software
/// timer in [`BusUart::recv`] is what decides where a frame ends.
const RX_IDLE_SYMBOLS: u8 = 2;

/// Reads at most this many bytes per `read_async` call.
const CHUNK: usize = 64;

/// What one [`BusUart::recv`] call produced.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rx {
    /// A complete frame is in [`BusUart::frame`].
    Frame,
    /// A UART receive error (framing, parity, noise, overrun).
    /// [`BusUart::frame`] may hold a partial frame worth logging.
    Error,
    /// The deadline passed with nothing in hand.
    Deadline,
}

/// UART1 settings for a bus configuration.
fn uart_config(bus: BusConfig) -> UartConfig {
    let (parity, stop_bits) = match bus.format {
        sniffer::BusFormat::N1 => (Parity::None, StopBits::_1),
        sniffer::BusFormat::E1 => (Parity::Even, StopBits::_1),
        sniffer::BusFormat::O1 => (Parity::Odd, StopBits::_1),
        sniffer::BusFormat::N2 => (Parity::None, StopBits::_2),
    };
    UartConfig::default()
        .with_baudrate(bus.baud)
        .with_data_bits(DataBits::_8)
        .with_parity(parity)
        .with_stop_bits(stop_bits)
        .with_rx(
            RxConfig::default()
                .with_fifo_full_threshold(RX_FIFO_THRESHOLD)
                .with_timeout(RX_IDLE_SYMBOLS),
        )
}

/// `true` if `frame` is long enough to carry a CRC and that CRC checks out.
#[must_use]
pub fn crc_ok(frame: &[u8]) -> bool {
    if frame.len() < 4 {
        return false;
    }
    let (body, crc) = frame.split_at(frame.len() - 2);
    u16::from_le_bytes([crc[0], crc[1]]) == sniffer::crc16(body)
}

/// One step of the receive loop, decided while the read future is still alive
/// and acted on once it is gone.
enum Step {
    Push(usize),
    Done(Rx),
    Wait,
}

/// Append received bytes, restarting the frame if it would overflow.
fn push_bytes(frame: &mut heapless::Vec<u8, { sniffer::MAX_FRAME_LEN }>, bytes: &[u8]) {
    for &byte in bytes {
        if frame.push(byte).is_err() {
            frame.clear();
            let _ = frame.push(byte);
        }
    }
}

/// UART1 with RX and TX assigned, plus the t3.5 framer.
pub struct BusUart {
    uart: Uart<'static, Async>,
    bus: BusConfig,
    t35: Duration,
    frame: heapless::Vec<u8, { sniffer::MAX_FRAME_LEN }>,
    chunk: [u8; CHUNK],
    last_tx: heapless::Vec<u8, { sniffer::MAX_FRAME_LEN }>,
    echo_pending: bool,
}

impl BusUart {
    /// Take the peripheral and the two pins for good.
    ///
    /// `with_rx` also enables the pin's internal pull-up, so a disconnected or
    /// loose RX wire idles high (silence) instead of floating into noise
    /// frames.
    ///
    /// This uses the full `Uart` driver rather than `UartRx`/`UartTx` on
    /// purpose: only `Uart::apply_config` applies the *common* settings (baud
    /// rate, data bits, parity, stop bits). `UartRx::apply_config` touches the
    /// RX-side settings only, so a `bus` command through it changed nothing and
    /// the next frames came back as parity errors and one-byte SHORT lines -
    /// seen on hardware with the sniffer.
    pub fn new(
        uart: esp_hal::peripherals::UART1<'static>,
        rx_pin: RxPin,
        tx_pin: TxPin,
        bus: BusConfig,
    ) -> Self {
        let uart = Uart::new(uart, uart_config(bus))
            .unwrap()
            .with_rx(rx_pin)
            .with_tx(tx_pin)
            .into_async();
        Self {
            uart,
            bus,
            t35: Duration::from_micros(bus.frame_timeout_us()),
            frame: heapless::Vec::new(),
            chunk: [0u8; CHUNK],
            last_tx: heapless::Vec::new(),
            echo_pending: false,
        }
    }

    /// The configuration the UART is actually running with.
    pub fn config(&self) -> BusConfig {
        self.bus
    }

    /// Re-apply the UART configuration. `false` means the hardware refused it
    /// and the old configuration is still in force.
    pub fn apply(&mut self, bus: BusConfig) -> bool {
        if self.uart.apply_config(&uart_config(bus)).is_err() {
            return false;
        }
        self.bus = bus;
        self.t35 = Duration::from_micros(bus.frame_timeout_us());
        true
    }

    /// The frame (or frame fragment) the last [`recv`](Self::recv) produced.
    pub fn frame(&self) -> &[u8] {
        &self.frame
    }

    /// Receive one frame, or give up at `deadline`.
    ///
    /// Once bytes are in hand the deadline no longer applies: the t3.5 gap gets
    /// to close the frame first, which costs at most a few milliseconds and
    /// keeps frames whole.
    pub async fn recv(&mut self, deadline: Instant) -> Rx {
        self.frame.clear();

        // Split the borrows up front: the read future holds `uart` and `chunk`
        // while `frame` is being appended to.
        let Self {
            uart,
            frame,
            chunk,
            t35,
            ..
        } = self;
        let t35 = *t35;

        loop {
            // The futures are temporaries of this statement, so they are gone
            // before `frame` is touched below.
            let step = if frame.is_empty() {
                match select(uart.read_async(chunk), Timer::at(deadline)).await {
                    Either::First(Ok(n)) => Step::Push(n),
                    Either::First(Err(_)) => Step::Done(Rx::Error),
                    Either::Second(()) => Step::Done(Rx::Deadline),
                }
            } else {
                // Timer::after restarts on every read, exactly like the
                // sniffer's receiver: silence of t3.5 ends the frame.
                match select3(
                    uart.read_async(chunk),
                    Timer::after(t35),
                    Timer::at(deadline),
                )
                .await
                {
                    Either3::First(Ok(n)) => Step::Push(n),
                    Either3::First(Err(_)) => Step::Done(Rx::Error),
                    Either3::Second(()) => Step::Done(Rx::Frame),
                    // A frame in hand beats the deadline; the gap timer above
                    // is what ends it. Looping re-arms both timers.
                    Either3::Third(()) => Step::Wait,
                }
            };

            match step {
                Step::Push(n) => push_bytes(frame, &chunk[..n]),
                Step::Done(rx) => return rx,
                Step::Wait => {}
            }
        }
    }

    /// Send one frame and wait for the last bit to leave the shift register.
    ///
    /// The frame goes into the capture stream as a `dir=tx` line, stamped when
    /// the transmission started.
    pub async fn send(&mut self, frame: &[u8]) -> bool {
        let timestamp_ms = Instant::now().as_millis();
        self.last_tx.clear();
        let _ = self.last_tx.extend_from_slice(frame);
        self.echo_pending = true;

        let mut rest = frame;
        let mut ok = true;
        while !rest.is_empty() {
            match self.uart.write_async(rest).await {
                Ok(0) => {
                    ok = false;
                    break;
                }
                Ok(n) => rest = &rest[n..],
                Err(_) => {
                    ok = false;
                    break;
                }
            }
        }
        if ok && self.uart.flush_async().await.is_err() {
            ok = false;
        }

        crate::publish_tx_frame(frame, timestamp_ms);
        ok
    }

    /// `true` if the frame in hand is the echo of our own last transmission.
    ///
    /// Consumes the expectation: a second identical frame is not our echo.
    pub fn take_echo(&mut self) -> bool {
        // Under four bytes it is noise, not an echo: a one-byte fragment that
        // happens to match our slave address should still show up as a SHORT
        // line, exactly as it does in the sniffer.
        if !self.echo_pending || self.frame.len() < 4 {
            return false;
        }
        if self.last_tx.starts_with(&self.frame) {
            self.echo_pending = false;
            return true;
        }
        false
    }

    /// Forget any outstanding echo expectation (after a response arrived, or
    /// when a request is abandoned).
    pub fn clear_echo(&mut self) {
        self.echo_pending = false;
    }
}
