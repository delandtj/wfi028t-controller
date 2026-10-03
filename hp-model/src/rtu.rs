//! Modbus RTU master framing for the heat pump bus.
//!
//! Only what the stock controller does (plus function 0x06, for the bench
//! question "does the heat pump accept single-register writes"):
//!
//! | Frame | Bytes |
//! |---|---|
//! | read status | `01 03 0000 003f <crc>` |
//! | read settings | `01 03 003f 0043 <crc>` |
//! | write settings | `01 10 003f 0043 86 <134 bytes> <crc>` |
//! | write single | `01 06 <reg> <value> <crc>` |
//!
//! Requests are returned by value as fixed-size arrays (the largest is 143
//! bytes) so nothing here needs an allocator or a scratch buffer from the
//! caller. Responses are parsed from a borrowed slice; register words are
//! copied into a caller-owned array.

/// Modbus slave address of the heat pump PCB.
pub const SLAVE: u8 = 0x01;

/// First register of the status block.
pub const STATUS_START: u16 = 0x0000;
/// Number of registers in the status block.
pub const STATUS_LEN: usize = 63;

/// First register of the settings block.
pub const SETTINGS_START: u16 = 0x003f;
/// Number of registers in the settings block.
pub const SETTINGS_LEN: usize = 67;

/// Read holding registers.
pub const FUNC_READ_HOLDING: u8 = 0x03;
/// Write single holding register.
pub const FUNC_WRITE_SINGLE: u8 = 0x06;
/// Write multiple holding registers.
pub const FUNC_WRITE_MULTIPLE: u8 = 0x10;

/// Length of a read or write-single request frame.
pub const REQUEST_LEN: usize = 8;
/// Length of a write-multiple ack frame (it echoes start and count).
pub const WRITE_ACK_LEN: usize = 8;

/// Length of the whole-block settings write frame: 9 bytes of framing plus
/// 134 bytes of register data.
pub const SETTINGS_WRITE_LEN: usize = 9 + 2 * SETTINGS_LEN;

/// Length of the heat pump's reply to a status block read.
pub const STATUS_RESPONSE_LEN: usize = 5 + 2 * STATUS_LEN;
/// Length of the heat pump's reply to a settings block read.
pub const SETTINGS_RESPONSE_LEN: usize = 5 + 2 * SETTINGS_LEN;

/// Everything that can go wrong between "we sent a frame" and "we have the
/// registers".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Error {
    /// Frame shorter than the smallest valid response, or its length does not
    /// match the function and byte count.
    BadLength,
    /// CRC over the frame body does not match the trailing CRC.
    BadCrc,
    /// Answer came from another address than [`SLAVE`].
    WrongSlave(u8),
    /// Function code is neither the one we asked for nor its exception form.
    WrongFunction(u8),
    /// Byte count field disagrees with the number of registers we asked for.
    BadByteCount(u8),
    /// A write ack echoed a different start address or register count.
    EchoMismatch,
    /// The heat pump answered `function | 0x80` with this exception code.
    Exception(u8),
    /// Caller asked to encode more registers than fit the destination buffer.
    BufferTooSmall,
    /// Register count outside what function 0x10 can carry (1..=123).
    BadRegisterCount(usize),
}

/// Modbus RTU CRC-16 (reflected, polynomial 0x8005, init 0xFFFF, low byte
/// sent first).
///
/// Deliberately local rather than taken from `modbus-sniffer-core`: that
/// crate would pull `embassy-sync`, `critical-section`, `futures-core` and
/// two major versions of `heapless` into what is otherwise a dependency-free
/// data model. The algorithm is ten lines and is pinned by test vectors.
#[must_use]
pub fn crc16(data: &[u8]) -> u16 {
    let mut crc: u16 = 0xffff;
    for &byte in data {
        crc ^= u16::from(byte);
        for _ in 0..8 {
            let lsb = crc & 1;
            crc >>= 1;
            if lsb != 0 {
                crc ^= 0xa001;
            }
        }
    }
    crc
}

/// Append the CRC of `frame[..body]` to `frame[body..body + 2]`.
fn seal(frame: &mut [u8], body: usize) {
    let crc = crc16(&frame[..body]);
    frame[body] = crc as u8;
    frame[body + 1] = (crc >> 8) as u8;
}

/// Verify the trailing CRC and the slave address, then return the PDU
/// (function code onwards, CRC stripped).
fn body(frame: &[u8]) -> Result<&[u8], Error> {
    // Smallest valid response is an exception: slave, func, code, crc, crc.
    if frame.len() < 5 {
        return Err(Error::BadLength);
    }
    let (payload, crc) = frame.split_at(frame.len() - 2);
    if u16::from_le_bytes([crc[0], crc[1]]) != crc16(payload) {
        return Err(Error::BadCrc);
    }
    if payload[0] != SLAVE {
        return Err(Error::WrongSlave(payload[0]));
    }
    Ok(&payload[1..])
}

/// Classify the function code of a verified PDU.
fn expect_function(pdu: &[u8], want: u8) -> Result<(), Error> {
    let got = pdu[0];
    if got == want {
        Ok(())
    } else if got == want | 0x80 {
        // Exception responses carry one code byte.
        match pdu.get(1) {
            Some(&code) => Err(Error::Exception(code)),
            None => Err(Error::BadLength),
        }
    } else {
        Err(Error::WrongFunction(got))
    }
}

/// Build `01 03 <start> <count> <crc>`.
#[must_use]
pub fn read_request(start: u16, count: u16) -> [u8; REQUEST_LEN] {
    let mut f = [0u8; REQUEST_LEN];
    f[0] = SLAVE;
    f[1] = FUNC_READ_HOLDING;
    f[2..4].copy_from_slice(&start.to_be_bytes());
    f[4..6].copy_from_slice(&count.to_be_bytes());
    seal(&mut f, 6);
    f
}

/// Build the status block read: `01 03 0000 003f <crc>`.
#[must_use]
pub fn read_status_request() -> [u8; REQUEST_LEN] {
    read_request(STATUS_START, STATUS_LEN as u16)
}

/// Build the settings block read: `01 03 003f 0043 <crc>`.
#[must_use]
pub fn read_settings_request() -> [u8; REQUEST_LEN] {
    read_request(SETTINGS_START, SETTINGS_LEN as u16)
}

/// Build `01 06 <reg> <value> <crc>`.
///
/// The stock controller never does this; it exists for the bench test that
/// asks whether the PCB accepts single-register writes at all.
#[must_use]
pub fn write_single_request(reg: u16, value: u16) -> [u8; REQUEST_LEN] {
    let mut f = [0u8; REQUEST_LEN];
    f[0] = SLAVE;
    f[1] = FUNC_WRITE_SINGLE;
    f[2..4].copy_from_slice(&reg.to_be_bytes());
    f[4..6].copy_from_slice(&value.to_be_bytes());
    seal(&mut f, 6);
    f
}

/// Build `01 10 <start> <count> <2*count> <data> <crc>` into `buf`, and
/// return the frame slice.
///
/// Used for the settings block through [`write_settings_request`]; exposed
/// for the bench's "0x10 with count 1" method.
///
/// # Errors
///
/// [`Error::BadRegisterCount`] if `regs` is empty or longer than the 123
/// registers function 0x10 can carry, [`Error::BufferTooSmall`] if `buf`
/// cannot hold `9 + 2 * regs.len()` bytes.
pub fn write_multiple_request<'b>(
    start: u16,
    regs: &[u16],
    buf: &'b mut [u8],
) -> Result<&'b [u8], Error> {
    if regs.is_empty() || regs.len() > 123 {
        return Err(Error::BadRegisterCount(regs.len()));
    }
    let len = 9 + 2 * regs.len();
    if buf.len() < len {
        return Err(Error::BufferTooSmall);
    }
    let f = &mut buf[..len];
    f[0] = SLAVE;
    f[1] = FUNC_WRITE_MULTIPLE;
    f[2..4].copy_from_slice(&start.to_be_bytes());
    f[4..6].copy_from_slice(&(regs.len() as u16).to_be_bytes());
    f[6] = 2 * regs.len() as u8;
    for (i, &w) in regs.iter().enumerate() {
        f[7 + 2 * i..9 + 2 * i].copy_from_slice(&w.to_be_bytes());
    }
    seal(f, len - 2);
    Ok(f)
}

/// Build the whole-block settings write the stock controller uses:
/// `01 10 003f 0043 86 <134 bytes> <crc>`.
#[must_use]
pub fn write_settings_request(regs: &[u16; SETTINGS_LEN]) -> [u8; SETTINGS_WRITE_LEN] {
    let mut f = [0u8; SETTINGS_WRITE_LEN];
    // Cannot fail: 67 registers are within 1..=123 and the buffer is sized
    // for exactly this block. Asserted so a future constant change is loud.
    let built = write_multiple_request(SETTINGS_START, regs, &mut f);
    debug_assert!(built.is_ok(), "settings write frame constants disagree");
    f
}

/// Copy the registers of a function 0x03 response into `out`.
///
/// `out.len()` is the register count that was requested; the byte count and
/// frame length must agree with it.
///
/// # Errors
///
/// [`Error::BadCrc`], [`Error::WrongSlave`], [`Error::WrongFunction`] or
/// [`Error::Exception`] for a frame that is not the reply we asked for, and
/// [`Error::BadByteCount`] or [`Error::BadLength`] when the register count
/// does not match `out`.
pub fn parse_read_response(frame: &[u8], out: &mut [u16]) -> Result<(), Error> {
    let pdu = body(frame)?;
    expect_function(pdu, FUNC_READ_HOLDING)?;
    if pdu.len() < 2 {
        return Err(Error::BadLength);
    }
    let byte_count = pdu[1];
    let want = 2 * out.len();
    if usize::from(byte_count) != want {
        return Err(Error::BadByteCount(byte_count));
    }
    let data = &pdu[2..];
    if data.len() != want {
        return Err(Error::BadLength);
    }
    let (pairs, _) = data.as_chunks::<2>();
    for (word, pair) in out.iter_mut().zip(pairs) {
        *word = u16::from_be_bytes(*pair);
    }
    Ok(())
}

/// Read a function 0x03 response of a known register count into a fresh array.
///
/// # Errors
///
/// As [`parse_read_response`], with `N` as the expected register count.
pub fn parse_read_block<const N: usize>(frame: &[u8]) -> Result<[u16; N], Error> {
    let mut out = [0u16; N];
    parse_read_response(frame, &mut out)?;
    Ok(out)
}

/// Check the ack of a function 0x10 write: `01 10 <start> <count> <crc>`.
///
/// # Errors
///
/// As [`parse_read_response`] for the framing checks, plus
/// [`Error::EchoMismatch`] if the ack echoes a different start or count.
pub fn parse_write_multiple_ack(frame: &[u8], start: u16, count: u16) -> Result<(), Error> {
    let pdu = body(frame)?;
    expect_function(pdu, FUNC_WRITE_MULTIPLE)?;
    if pdu.len() != 5 {
        return Err(Error::BadLength);
    }
    if u16::from_be_bytes([pdu[1], pdu[2]]) != start
        || u16::from_be_bytes([pdu[3], pdu[4]]) != count
    {
        return Err(Error::EchoMismatch);
    }
    Ok(())
}

/// Check the ack of the whole-block settings write.
///
/// # Errors
///
/// As [`parse_write_multiple_ack`].
pub fn parse_settings_write_ack(frame: &[u8]) -> Result<(), Error> {
    parse_write_multiple_ack(frame, SETTINGS_START, SETTINGS_LEN as u16)
}

/// Check the ack of a function 0x06 write: the request is echoed verbatim.
///
/// # Errors
///
/// As [`parse_write_multiple_ack`], against the register and value we sent.
pub fn parse_write_single_ack(frame: &[u8], reg: u16, value: u16) -> Result<(), Error> {
    let pdu = body(frame)?;
    expect_function(pdu, FUNC_WRITE_SINGLE)?;
    if pdu.len() != 5 {
        return Err(Error::BadLength);
    }
    if u16::from_be_bytes([pdu[1], pdu[2]]) != reg || u16::from_be_bytes([pdu[3], pdu[4]]) != value
    {
        return Err(Error::EchoMismatch);
    }
    Ok(())
}

#[cfg(test)]
pub(crate) mod test_support {
    use super::{crc16, FUNC_READ_HOLDING, SLAVE};

    /// Build the heat pump's function 0x03 reply for `regs` into `buf`,
    /// returning the frame length.
    pub fn read_response(regs: &[u16], buf: &mut [u8]) -> usize {
        let len = 5 + 2 * regs.len();
        buf[0] = SLAVE;
        buf[1] = FUNC_READ_HOLDING;
        buf[2] = 2 * regs.len() as u8;
        for (i, &w) in regs.iter().enumerate() {
            buf[3 + 2 * i..5 + 2 * i].copy_from_slice(&w.to_be_bytes());
        }
        let crc = crc16(&buf[..len - 2]);
        buf[len - 2] = crc as u8;
        buf[len - 1] = (crc >> 8) as u8;
        len
    }

    /// Build an exception reply: `01 <func|0x80> <code> <crc>`.
    pub fn exception(func: u8, code: u8) -> [u8; 5] {
        let mut f = [SLAVE, func | 0x80, code, 0, 0];
        let crc = crc16(&f[..3]);
        f[3] = crc as u8;
        f[4] = (crc >> 8) as u8;
        f
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Second, structurally different CRC implementation: it processes each
    /// byte MSB-first with the unreflected polynomial 0x8005 on bit-reversed
    /// input and reverses the result, instead of the usual reflected
    /// right-shift loop. Agreement between the two plus the published
    /// `01 03 00 00 00 0a -> c5 cd` vector pins the function down.
    fn crc16_ref(data: &[u8]) -> u16 {
        let mut crc: u16 = 0xffff;
        for &b in data {
            crc ^= u16::from(b.reverse_bits()) << 8;
            for _ in 0..8 {
                if crc & 0x8000 != 0 {
                    crc = (crc << 1) ^ 0x8005;
                } else {
                    crc <<= 1;
                }
            }
        }
        crc.reverse_bits()
    }

    #[test]
    fn crc_known_modbus_vector() {
        // Published Modbus example, also confirmed by the sniffer handoff.
        let f = [0x01u8, 0x03, 0x00, 0x00, 0x00, 0x0a];
        let crc = crc16(&f);
        assert_eq!([crc as u8, (crc >> 8) as u8], [0xc5, 0xcd]);
        assert_eq!(crc, crc16_ref(&f));
    }

    #[test]
    fn crc_agrees_with_independent_implementation() {
        assert_eq!(crc16(&[]), 0xffff);
        assert_eq!(crc16(&[]), crc16_ref(&[]));
        let mut data = [0u8; 143];
        for (i, b) in data.iter_mut().enumerate() {
            *b = (i as u8).wrapping_mul(37).wrapping_add(11);
        }
        for n in 0..=data.len() {
            assert_eq!(crc16(&data[..n]), crc16_ref(&data[..n]), "len {n}");
        }
    }

    #[test]
    fn status_read_request_is_exact() {
        let req = read_status_request();
        let crc = crc16_ref(&[0x01, 0x03, 0x00, 0x00, 0x00, 0x3f]);
        assert_eq!(
            req,
            [
                0x01,
                0x03,
                0x00,
                0x00,
                0x00,
                0x3f,
                crc as u8,
                (crc >> 8) as u8
            ]
        );
    }

    #[test]
    fn settings_read_request_is_exact() {
        let req = read_settings_request();
        assert_eq!(&req[..6], &[0x01, 0x03, 0x00, 0x3f, 0x00, 0x43]);
        assert_eq!(
            u16::from_le_bytes([req[6], req[7]]),
            crc16_ref(&[0x01, 0x03, 0x00, 0x3f, 0x00, 0x43])
        );
    }

    #[test]
    fn write_single_request_is_exact() {
        let req = write_single_request(0x0041, 34);
        assert_eq!(&req[..6], &[0x01, 0x06, 0x00, 0x41, 0x00, 0x22]);
        assert_eq!(u16::from_le_bytes([req[6], req[7]]), crc16_ref(&req[..6]));
    }

    #[test]
    fn settings_write_request_header_and_crc() {
        let regs = [0x1071u16; SETTINGS_LEN];
        let f = write_settings_request(&regs);
        assert_eq!(f.len(), 143);
        assert_eq!(&f[..7], &[0x01, 0x10, 0x00, 0x3f, 0x00, 0x43, 0x86]);
        assert_eq!(f[7], 0x10);
        assert_eq!(f[8], 0x71);
        assert_eq!(u16::from_le_bytes([f[141], f[142]]), crc16_ref(&f[..141]));
    }

    #[test]
    fn write_multiple_rejects_bad_sizes() {
        let mut buf = [0u8; 16];
        assert_eq!(
            write_multiple_request(0x0041, &[], &mut buf),
            Err(Error::BadRegisterCount(0))
        );
        let big = [0u16; 124];
        assert_eq!(
            write_multiple_request(0x0041, &big, &mut buf),
            Err(Error::BadRegisterCount(124))
        );
        let mut small = [0u8; 10];
        assert_eq!(
            write_multiple_request(0x0041, &[1, 2], &mut small),
            Err(Error::BufferTooSmall)
        );
        let mut expected = [0x01u8, 0x10, 0x00, 0x41, 0x00, 0x01, 0x02, 0x00, 0x22, 0, 0];
        let crc = crc16_ref(&expected[..9]);
        expected[9] = crc as u8;
        expected[10] = (crc >> 8) as u8;
        let frame = write_multiple_request(0x0041, &[34], &mut buf).unwrap();
        assert_eq!(frame, &expected[..]);
    }

    #[test]
    fn round_trip_read_response() {
        let regs = [0x0000u16, 0x7fff, 0x1234, 0xffff];
        let mut buf = [0u8; 32];
        let n = test_support::read_response(&regs, &mut buf);
        let got: [u16; 4] = parse_read_block(&buf[..n]).unwrap();
        assert_eq!(got, regs);
    }

    #[test]
    fn bad_crc_is_classified() {
        let regs = [1u16, 2, 3];
        let mut buf = [0u8; 32];
        let n = test_support::read_response(&regs, &mut buf);
        buf[n - 1] ^= 0xff;
        let mut out = [0u16; 3];
        assert_eq!(parse_read_response(&buf[..n], &mut out), Err(Error::BadCrc));
    }

    #[test]
    fn exception_is_classified() {
        let f = test_support::exception(FUNC_READ_HOLDING, 0x02);
        let mut out = [0u16; 3];
        assert_eq!(parse_read_response(&f, &mut out), Err(Error::Exception(2)));

        let f = test_support::exception(FUNC_WRITE_MULTIPLE, 0x03);
        assert_eq!(parse_settings_write_ack(&f), Err(Error::Exception(3)));
    }

    #[test]
    fn wrong_slave_and_function_are_classified() {
        let regs = [1u16];
        let mut buf = [0u8; 16];
        let n = test_support::read_response(&regs, &mut buf);

        let mut wrong_slave = buf;
        wrong_slave[0] = 0x02;
        seal(&mut wrong_slave, n - 2);
        let mut out = [0u16; 1];
        assert_eq!(
            parse_read_response(&wrong_slave[..n], &mut out),
            Err(Error::WrongSlave(2))
        );

        // A 0x03 reply arriving where a 0x10 ack was expected.
        assert_eq!(
            parse_write_multiple_ack(&buf[..n], SETTINGS_START, SETTINGS_LEN as u16),
            Err(Error::WrongFunction(FUNC_READ_HOLDING))
        );
    }

    #[test]
    fn short_and_mismatched_frames_are_classified() {
        let mut out = [0u16; 1];
        assert_eq!(
            parse_read_response(&[0x01, 0x03], &mut out),
            Err(Error::BadLength)
        );

        let regs = [1u16, 2, 3];
        let mut buf = [0u8; 32];
        let n = test_support::read_response(&regs, &mut buf);
        // We asked for 4 registers, got 3.
        let mut four = [0u16; 4];
        assert_eq!(
            parse_read_response(&buf[..n], &mut four),
            Err(Error::BadByteCount(6))
        );
    }

    #[test]
    fn write_acks_are_checked() {
        // The real ack seen on the bus: 01 10 003f 0043 + crc.
        let mut ack = [0x01u8, 0x10, 0x00, 0x3f, 0x00, 0x43, 0, 0];
        seal(&mut ack, 6);
        assert_eq!(parse_settings_write_ack(&ack), Ok(()));

        let mut wrong = [0x01u8, 0x10, 0x00, 0x40, 0x00, 0x43, 0, 0];
        seal(&mut wrong, 6);
        assert_eq!(parse_settings_write_ack(&wrong), Err(Error::EchoMismatch));

        let req = write_single_request(0x0041, 34);
        assert_eq!(parse_write_single_ack(&req, 0x0041, 34), Ok(()));
        assert_eq!(
            parse_write_single_ack(&req, 0x0041, 35),
            Err(Error::EchoMismatch)
        );
    }
}
