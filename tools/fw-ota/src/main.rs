//! `fw-ota` - sign a controller firmware image and push it over the network.
//!
//! ADR 0002, component 7. Two subcommands:
//!
//! ```text
//! fw-ota keygen                 create the signing key pair (once, ever)
//! fw-ota push <host> [--wait]   build, sign and send an image
//! ```
//!
//! The private key lives OUTSIDE this repository, at
//! `~/.config/wfi028t/ota-signing.key` (mode 0600). The public half is
//! committed as `fw/ota-signing.pub` and compiled into the firmware with
//! `include_bytes!`, so only an image signed with that one key is accepted.
//! Lose the key and the next update needs a USB cable (see the README).
//!
//! The wire format lives in one file, shared verbatim with the firmware:
//! `fw/src/ota/header.rs`, included below. `cargo test -p fw-ota` runs its
//! unit tests on the host - that is the only place they run, the firmware
//! crate being a `no_std` binary.

use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};
use std::time::{Duration, Instant};

use ed25519_dalek::{Signer, SigningKey};
use sha2::{Digest, Sha256};

// The device side of the wire format, verbatim: one file, two compilers.
// Parts of it (the receiver's half: `parse`, `accept`) are only exercised by
// the tests here, which is the point - this tool proves them against the
// signatures it produces.
#[path = "../../../fw/src/ota/header.rs"]
#[allow(dead_code)]
mod header;

use header::{Header, PUBLIC_KEY_LEN, SIGNED_LEN, TARGET};

/// Where the OTA receiver listens (ADR 0002).
const OTA_PORT: u16 = 4002;

/// The console port, used by `--wait` to read `status` back.
const CONSOLE_PORT: u16 = 4001;

/// Image chunk handed to the kernel at a time. One flash sector, which is
/// also what the device erases and writes in one go.
const CHUNK: usize = 4096;

/// How long a single network operation may block.
const IO_TIMEOUT: Duration = Duration::from_secs(60);

/// How long `--wait` keeps asking for a verdict. The device confirms or
/// resets within 120 s of boot (`fw/src/ota.rs`), plus a boot and a DHCP
/// lease, plus slack.
const WAIT_LIMIT: Duration = Duration::from_secs(210);

/// Gap between `--wait` polls.
const WAIT_POLL: Duration = Duration::from_secs(5);

/// The chip the firmware runs on, for `espflash save-image`.
const CHIP: &str = "esp32c6";

const USAGE: &str = "\
usage:
  fw-ota keygen [--key <path>] [--pub <path>]
  fw-ota push <host> [--port <n>] [--elf <path> | --image <path>]
                     [--fw-version <text>] [--key <path>] [--wait]

  keygen  Create the ed25519 signing key pair. Writes the private key to
          ~/.config/wfi028t/ota-signing.key (mode 0600) and the public key to
          fw/ota-signing.pub, which is committed and compiled into the
          firmware. Refuses to overwrite either file.

  push    Build an image from the release ELF (espflash save-image), sign a
          header for it and send it to <host> on port 4002. Prints what the
          device says; exits 0 only on `ok rebooting`. With --wait, keeps
          asking the console port 4001 for `status` afterwards until the new
          image confirms itself (ota=valid) or is rolled back.
";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let result = match args.first().map(String::as_str) {
        Some("keygen") => keygen(&args[1..]),
        Some("push") => push(&args[1..]),
        Some("-h") | Some("--help") | Some("help") => {
            print!("{USAGE}");
            return ExitCode::SUCCESS;
        }
        _ => {
            eprint!("{USAGE}");
            return ExitCode::FAILURE;
        }
    };

    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(reason) => {
            eprintln!("fw-ota: {reason}");
            ExitCode::FAILURE
        }
    }
}

// ---------------------------------------------------------------------------
// Paths
// ---------------------------------------------------------------------------

/// This repository, as of the build of this tool. `fw-ota` is a workspace
/// crate run from a checkout, so this is a dependable anchor for the ELF and
/// the public key - and `--elf` / `--pub` override it anyway.
fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .unwrap_or(Path::new("."))
        .to_path_buf()
}

/// `~/.config/wfi028t/ota-signing.key`, honouring `XDG_CONFIG_HOME`.
fn default_key_path() -> Result<PathBuf, String> {
    let base = match std::env::var_os("XDG_CONFIG_HOME") {
        Some(dir) if !dir.is_empty() => PathBuf::from(dir),
        _ => {
            let home = std::env::var_os("HOME").ok_or("HOME is not set")?;
            PathBuf::from(home).join(".config")
        }
    };
    Ok(base.join("wfi028t").join("ota-signing.key"))
}

fn default_pub_path() -> PathBuf {
    repo_root().join("fw").join("ota-signing.pub")
}

/// The release ELF `cargo build --release` leaves behind. `CARGO_TARGET_DIR`
/// is honoured, because a shared target directory is a common setup and the
/// ELF is then nowhere near the checkout.
fn default_elf_path() -> PathBuf {
    let target = match std::env::var_os("CARGO_TARGET_DIR") {
        Some(dir) if !dir.is_empty() => PathBuf::from(dir),
        _ => repo_root().join("target"),
    };
    target
        .join("riscv32imac-unknown-none-elf")
        .join("release")
        .join("wfi-controller-fw")
}

/// The firmware's build stamp, worked out the way `fw/build.rs` does it
/// (`<crate version>+<commit>[-dirty]`), so a push labels the image with what
/// the device will report in its hello line. The header field holds
/// [`header::FW_VERSION_LEN`] bytes; a stamp that does not fit drops the crate
/// version (`fd17171-dirty`), which is the part that never changes.
fn firmware_version() -> String {
    let stamp = match git_stamp() {
        Some(commit) => format!("{}+{commit}", crate_version()),
        None => crate_version(),
    };
    if stamp.len() <= header::FW_VERSION_LEN {
        return stamp;
    }
    match stamp.split_once('+') {
        Some((_, commit)) => commit.to_string(),
        None => stamp,
    }
}

/// `<commit>[-dirty]` of the firmware sources, `None` outside a checkout.
/// Same sources as `fw/build.rs`'s `SOURCES`.
fn git_stamp() -> Option<String> {
    let fw = repo_root().join("fw");
    let git = |args: &[&str]| -> Option<String> {
        let out = Command::new("git")
            .current_dir(&fw)
            .args(args)
            .output()
            .ok()?;
        out.status
            .success()
            .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
    };
    let commit = git(&["rev-parse", "--short=7", "HEAD"])?;
    let dirty = git(&[
        "status",
        "--porcelain",
        "--untracked-files=no",
        "--",
        ".",
        "../hp-model",
        "../Cargo.toml",
        "../Cargo.lock",
    ])
    .is_some_and(|changes| !changes.is_empty());
    Some(if dirty {
        format!("{commit}-dirty")
    } else {
        commit
    })
}

/// The `version` of `fw/Cargo.toml`'s `[package]`, or `unknown`.
fn crate_version() -> String {
    let manifest = repo_root().join("fw").join("Cargo.toml");
    let Ok(text) = fs::read_to_string(&manifest) else {
        return "unknown".to_string();
    };
    for line in text.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("version") {
            let rest = rest.trim_start();
            if let Some(rest) = rest.strip_prefix('=') {
                return rest.trim().trim_matches('"').to_string();
            }
        }
        if line == "[dependencies]" {
            break;
        }
    }
    "unknown".to_string()
}

// ---------------------------------------------------------------------------
// Arguments
// ---------------------------------------------------------------------------

/// Pull `--name <value>` options out of `args`, leaving the positionals.
struct Options {
    flags: Vec<(String, Option<String>)>,
    positional: Vec<String>,
}

impl Options {
    fn parse(args: &[String], valueless: &[&str]) -> Result<Self, String> {
        let mut flags = Vec::new();
        let mut positional = Vec::new();
        let mut rest = args.iter();
        while let Some(arg) = rest.next() {
            if let Some(name) = arg.strip_prefix("--") {
                if valueless.contains(&name) {
                    flags.push((name.to_string(), None));
                } else {
                    let value = rest
                        .next()
                        .ok_or_else(|| format!("--{name} needs a value"))?;
                    flags.push((name.to_string(), Some(value.clone())));
                }
            } else {
                positional.push(arg.clone());
            }
        }
        Ok(Self { flags, positional })
    }

    fn value(&self, name: &str) -> Option<&str> {
        self.flags
            .iter()
            .find(|(flag, _)| flag == name)
            .and_then(|(_, value)| value.as_deref())
    }

    fn present(&self, name: &str) -> bool {
        self.flags.iter().any(|(flag, _)| flag == name)
    }

    /// Reject anything not in `known`, so a typo is not silently ignored.
    fn reject_unknown(&self, known: &[&str]) -> Result<(), String> {
        for (flag, _) in &self.flags {
            if !known.contains(&flag.as_str()) {
                return Err(format!("unknown option --{flag}\n\n{USAGE}"));
            }
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// keygen
// ---------------------------------------------------------------------------

fn keygen(args: &[String]) -> Result<(), String> {
    let options = Options::parse(args, &[])?;
    options.reject_unknown(&["key", "pub"])?;

    let key_path = match options.value("key") {
        Some(path) => PathBuf::from(path),
        None => default_key_path()?,
    };
    let pub_path = match options.value("pub") {
        Some(path) => PathBuf::from(path),
        None => default_pub_path(),
    };

    // Refuse to overwrite either half: a new key pair silently replacing the
    // old one means every image already signed is worthless, and a public key
    // replaced without the private one means no update can ever be accepted.
    if key_path.exists() {
        return Err(format!(
            "{} exists; move it aside by hand if you really mean to replace the key",
            key_path.display()
        ));
    }
    if pub_path.exists() {
        return Err(format!(
            "{} exists; move it aside by hand if you really mean to replace the key",
            pub_path.display()
        ));
    }

    let mut seed = [0u8; 32];
    getrandom::fill(&mut seed).map_err(|err| format!("no randomness available: {err}"))?;
    let signing = SigningKey::from_bytes(&seed);
    let public = signing.verifying_key().to_bytes();

    if let Some(dir) = key_path.parent() {
        fs::create_dir_all(dir).map_err(|err| format!("{}: {err}", dir.display()))?;
        // The directory too: a world-readable ~/.config/wfi028t with a 0600
        // file in it is fine, but 0700 is one less thing to think about.
        let _ = fs::set_permissions(dir, fs::Permissions::from_mode(0o700));
    }

    // 0600 from the moment it exists, not after: create_new also means two
    // simultaneous keygens cannot both think they won.
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&key_path)
        .map_err(|err| format!("{}: {err}", key_path.display()))?;
    writeln!(file, "{}", hex(&seed)).map_err(|err| format!("{}: {err}", key_path.display()))?;
    drop(file);

    // The public key as raw bytes: the firmware takes it with include_bytes!.
    fs::write(&pub_path, public).map_err(|err| format!("{}: {err}", pub_path.display()))?;

    println!(
        "private key: {} (mode 0600, never commit)",
        key_path.display()
    );
    println!("public key:  {} (commit this)", pub_path.display());
    println!("public key:  {}", hex(&public));
    println!();
    println!("Back the private key up now - without it the next update needs USB.");
    println!("Rebuild and USB-flash the firmware so it carries the new public key.");
    Ok(())
}

fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

fn unhex(text: &str) -> Result<Vec<u8>, String> {
    let text = text.trim();
    if !text.len().is_multiple_of(2) {
        return Err("odd number of hex digits".to_string());
    }
    (0..text.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&text[i..i + 2], 16).map_err(|err| err.to_string()))
        .collect()
}

/// Load the private key. Accepts the hex form `keygen` writes and a raw
/// 32-byte file, so a key restored from a backup in either shape works.
fn load_signing_key(path: &Path) -> Result<SigningKey, String> {
    let raw = fs::read(path).map_err(|err| {
        format!(
            "{}: {err}\n(run `fw-ota keygen` once, or point --key at your backup)",
            path.display()
        )
    })?;

    let seed: [u8; 32] = if raw.len() == 32 {
        raw.try_into().expect("checked length")
    } else {
        let text = String::from_utf8(raw).map_err(|_| format!("{}: not a key", path.display()))?;
        unhex(&text)
            .map_err(|err| format!("{}: {err}", path.display()))?
            .try_into()
            .map_err(|_| format!("{}: not a 32-byte key", path.display()))?
    };

    if let Ok(meta) = fs::metadata(path) {
        let mode = meta.permissions().mode() & 0o077;
        if mode != 0 {
            eprintln!(
                "fw-ota: warning: {} is readable by others (mode {:o}); chmod 600 it",
                path.display(),
                meta.permissions().mode() & 0o777
            );
        }
    }

    Ok(SigningKey::from_bytes(&seed))
}

// ---------------------------------------------------------------------------
// push
// ---------------------------------------------------------------------------

fn push(args: &[String]) -> Result<(), String> {
    let options = Options::parse(args, &["wait"])?;
    options.reject_unknown(&["port", "elf", "image", "fw-version", "key", "wait"])?;

    let host = options
        .positional
        .first()
        .ok_or_else(|| format!("push needs a host\n\n{USAGE}"))?
        .clone();
    let port: u16 = match options.value("port") {
        Some(text) => text.parse().map_err(|_| "--port is not a port number")?,
        None => OTA_PORT,
    };

    let key_path = match options.value("key") {
        Some(path) => PathBuf::from(path),
        None => default_key_path()?,
    };
    let signing = load_signing_key(&key_path)?;

    let fw_version = match options.value("fw-version") {
        Some(text) => text.to_string(),
        None => firmware_version(),
    };

    // The image: either one that already exists, or one built from the ELF.
    let (image, source) = match options.value("image") {
        Some(path) => (
            fs::read(path).map_err(|err| format!("{path}: {err}"))?,
            path.to_string(),
        ),
        None => {
            let elf = match options.value("elf") {
                Some(path) => PathBuf::from(path),
                None => default_elf_path(),
            };
            let image = save_image(&elf)?;
            (image, elf.display().to_string())
        }
    };

    let image_len = u32::try_from(image.len()).map_err(|_| "image is absurdly large")?;
    let digest: [u8; 32] = Sha256::digest(&image).into();

    let mut parsed = Header::new(image_len, digest, TARGET, &fw_version)
        .map_err(|reject| format!("header: {}", reject.as_str()))?;
    let unsigned = parsed.encode();
    parsed.signature = signing.sign(&unsigned[..SIGNED_LEN]).to_bytes();
    let raw = parsed.encode();

    // Verify with the firmware's own verifier before a single byte goes out:
    // if this fails the device would have refused the push anyway, and the
    // reason is here, not in a log on the board.
    let public: [u8; PUBLIC_KEY_LEN] = signing.verifying_key().to_bytes();
    if !header::verify(&raw[..SIGNED_LEN], &parsed.signature, &public) {
        return Err("signed header does not verify against its own key".to_string());
    }

    println!("image:      {source}");
    println!("            {image_len} bytes, sha256 {}", hex(&digest));
    println!("fw_version: {fw_version}");
    println!("target:     {TARGET}");
    println!("key:        {} ({})", key_path.display(), hex(&public));
    println!("sending to  {host}:{port}");

    send(&host, port, &raw, &image)?;

    if options.present("wait") {
        wait_for_verdict(&host)?;
    }
    Ok(())
}

/// Turn the release ELF into the flashable image espflash would write.
fn save_image(elf: &Path) -> Result<Vec<u8>, String> {
    if !elf.exists() {
        return Err(format!(
            "{}: no such file\n(build it first: cd fw && cargo build --release)",
            elf.display()
        ));
    }

    let out = std::env::temp_dir().join(format!("fw-ota-{}.bin", std::process::id()));
    let status = Command::new("espflash")
        .arg("save-image")
        .arg("--chip")
        .arg(CHIP)
        .arg(elf)
        .arg(&out)
        .status()
        .map_err(|err| format!("espflash save-image: {err}"))?;
    if !status.success() {
        return Err(format!("espflash save-image failed ({status})"));
    }

    let image = fs::read(&out).map_err(|err| format!("{}: {err}", out.display()))?;
    let _ = fs::remove_file(&out);
    Ok(image)
}

/// Send header and image, printing every line the device answers.
///
/// The device talks back while the image is still being written (`ok header`,
/// progress every 64 KB), so the socket is read in a thread: a device whose
/// replies nobody drains would eventually block on its own small TX buffer.
fn send(host: &str, port: u16, raw_header: &[u8], image: &[u8]) -> Result<(), String> {
    let address = (host, port)
        .to_socket_addrs()
        .map_err(|err| format!("{host}:{port}: {err}"))?
        .next()
        .ok_or_else(|| format!("{host}:{port}: no address"))?;
    let stream = TcpStream::connect_timeout(&address, IO_TIMEOUT)
        .map_err(|err| format!("{host}:{port}: {err}"))?;
    stream.set_write_timeout(Some(IO_TIMEOUT)).ok();
    stream.set_read_timeout(Some(IO_TIMEOUT)).ok();

    let reader = stream
        .try_clone()
        .map_err(|err| format!("cannot read the socket: {err}"))?;
    let printer = std::thread::spawn(move || {
        let mut lines = Vec::new();
        for line in BufReader::new(reader).lines() {
            match line {
                Ok(line) => {
                    println!("  {line}");
                    lines.push(line);
                }
                Err(_) => break,
            }
        }
        lines
    });

    let mut writer = stream;
    let outcome = (|| -> Result<(), String> {
        writer
            .write_all(raw_header)
            .map_err(|err| format!("sending the header: {err}"))?;
        writer.flush().ok();
        for chunk in image.chunks(CHUNK) {
            writer
                .write_all(chunk)
                .map_err(|err| format!("sending the image: {err}"))?;
        }
        writer.flush().ok();
        Ok(())
    })();

    // Let the device finish talking (it closes the socket after `ok
    // rebooting` or after one `err ...`), then look at what it said.
    let lines = printer.join().unwrap_or_default();
    let said = |needle: &str| lines.iter().any(|line| line.contains(needle));

    if let Some(error) = lines.iter().find(|line| line.contains("err ")) {
        return Err(format!("device refused the image: {}", error.trim()));
    }
    // A write error only matters if the device did not already have a verdict:
    // it closes the socket as soon as it refuses, which makes our last write
    // fail for the right reason.
    outcome?;
    if !said("ok rebooting") {
        return Err("device did not say `ok rebooting`; nothing was activated".to_string());
    }
    println!("image accepted; the device is rebooting into it on probation");
    Ok(())
}

/// Ask the console port for `status` until the new image confirms itself.
///
/// ADR 0002 has this polling the capture daemon's control socket; the console
/// port does the same job without needing the daemon to be running, and it
/// is the port this release adds anyway.
fn wait_for_verdict(host: &str) -> Result<(), String> {
    println!("waiting for the new image to confirm itself (ota=valid)...");
    let deadline = Instant::now() + WAIT_LIMIT;
    let mut last = String::new();

    while Instant::now() < deadline {
        std::thread::sleep(WAIT_POLL);
        let status = match console_status(host) {
            Ok(status) => status,
            // The device is rebooting, or the AP has not handed it a lease
            // again yet: not an answer, keep asking.
            Err(_) => continue,
        };

        let state = field(&status, "ota=").unwrap_or("unknown").to_string();
        if state != last {
            println!("  ota={state}");
            last = state.clone();
        }
        match state.as_str() {
            "valid" => {
                println!("confirmed: the new image is now the one the bootloader keeps.");
                return Ok(());
            }
            "aborted" | "invalid" => {
                return Err(format!(
                    "the new image was rolled back (ota={state}); \
                     the previous image is running again"
                ));
            }
            _ => {}
        }
    }
    Err(format!(
        "no verdict within {} s; ask the device yourself: \
         printf 'status\\n' | nc {host} {CONSOLE_PORT}",
        WAIT_LIMIT.as_secs()
    ))
}

/// One `status` round trip on the console port.
fn console_status(host: &str) -> Result<String, String> {
    let address = (host, CONSOLE_PORT)
        .to_socket_addrs()
        .map_err(|err| err.to_string())?
        .next()
        .ok_or("no address")?;
    let mut stream = TcpStream::connect_timeout(&address, Duration::from_secs(5))
        .map_err(|err| err.to_string())?;
    stream.set_read_timeout(Some(Duration::from_secs(5))).ok();
    stream.set_write_timeout(Some(Duration::from_secs(5))).ok();
    stream
        .write_all(b"status\r\n")
        .map_err(|err| err.to_string())?;

    // The hello line comes first, then whatever the ring produces, then the
    // status line. Read for a moment and pick the status line out.
    let mut buf = [0u8; 4096];
    let mut text = String::new();
    let until = Instant::now() + Duration::from_secs(5);
    while Instant::now() < until {
        match stream.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                text.push_str(&String::from_utf8_lossy(&buf[..n]));
                if let Some(line) = text.lines().find(|line| line.contains(" ota=")) {
                    return Ok(line.to_string());
                }
            }
            Err(_) => break,
        }
    }
    Err("no status line".to_string())
}

/// The value of `name=` in a status line.
fn field<'a>(line: &'a str, name: &str) -> Option<&'a str> {
    line.split_whitespace()
        .find_map(|token| token.strip_prefix(name))
}

#[cfg(test)]
mod tests {
    use super::header::{self, Header, PUBLIC_KEY_LEN, SIGNED_LEN, TARGET};
    use super::{field, hex, unhex, Options};
    use ed25519_dalek::{Signer, SigningKey};

    /// The two ed25519 implementations have to agree: this tool signs with
    /// ed25519-dalek, the firmware verifies with ed25519-compact. If that
    /// ever stopped holding, every push would be refused on the board - and
    /// the board is the awkward place to find out.
    #[test]
    fn a_dalek_signature_satisfies_the_device_verifier() {
        let signing = SigningKey::from_bytes(&[3u8; 32]);
        let public: [u8; PUBLIC_KEY_LEN] = signing.verifying_key().to_bytes();

        let mut pushed = Header::new(755_000, [0x11; 32], TARGET, "9.9.9").unwrap();
        let unsigned = pushed.encode();
        pushed.signature = signing.sign(&unsigned[..SIGNED_LEN]).to_bytes();
        let raw = pushed.encode();

        let accepted = Header::accept(&raw, TARGET, &public, 0x40_0000).unwrap();
        assert_eq!(accepted.fw_version_name(), "9.9.9");
        assert_eq!(accepted.image_len, 755_000);
        assert!(header::verify(
            &raw[..SIGNED_LEN],
            &pushed.signature,
            &public
        ));

        // Another key, same header: refused.
        let other = SigningKey::from_bytes(&[4u8; 32])
            .verifying_key()
            .to_bytes();
        assert!(Header::accept(&raw, TARGET, &other, 0x40_0000).is_err());
    }

    /// The committed public key is the one the firmware compiles in, so it
    /// has to be exactly 32 bytes. A truncated or hex-encoded file here would
    /// be a firmware that refuses everything.
    #[test]
    fn the_committed_public_key_is_32_raw_bytes() {
        let path = super::default_pub_path();
        let raw = std::fs::read(&path).unwrap_or_else(|err| panic!("{}: {err}", path.display()));
        assert_eq!(raw.len(), PUBLIC_KEY_LEN);
        assert!(raw.iter().any(|&byte| byte != 0));
    }

    #[test]
    fn hex_round_trips() {
        let bytes = [0x00, 0x01, 0x7f, 0x80, 0xff];
        assert_eq!(hex(&bytes), "00017f80ff");
        assert_eq!(unhex("00017f80ff").unwrap(), bytes);
        assert_eq!(unhex(" 00017f80ff\n").unwrap(), bytes);
        assert!(unhex("abc").is_err());
        assert!(unhex("zz").is_err());
    }

    #[test]
    fn options_split_flags_from_positionals() {
        let args: Vec<String> = ["hp", "--port", "4002", "--wait", "--elf", "x.elf"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let options = Options::parse(&args, &["wait"]).unwrap();
        assert_eq!(options.positional, vec!["hp".to_string()]);
        assert_eq!(options.value("port"), Some("4002"));
        assert_eq!(options.value("elf"), Some("x.elf"));
        assert!(options.present("wait"));
        assert!(!options.present("image"));
        assert!(options.reject_unknown(&["port", "wait", "elf"]).is_ok());
        assert!(options.reject_unknown(&["port"]).is_err());
    }

    #[test]
    fn a_flag_without_its_value_is_an_error() {
        let args: Vec<String> = ["hp".to_string(), "--port".to_string()].to_vec();
        assert!(Options::parse(&args, &["wait"]).is_err());
    }

    #[test]
    fn status_fields_are_picked_out() {
        let line = "# status uptime=12.000 mode=master link=up ota=pending console=idle";
        assert_eq!(field(line, "ota="), Some("pending"));
        assert_eq!(field(line, "console="), Some("idle"));
        assert_eq!(field(line, "mode="), Some("master"));
        assert_eq!(field(line, "nothing="), None);
    }
}
