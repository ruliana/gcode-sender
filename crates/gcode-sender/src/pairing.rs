//! Pairing lets a browser on another computer use the sender, the way a smart
//! TV is paired: the machine shows a short code, and the browser enters it.
//! A correct code gives that browser a token. The browser keeps the token and
//! sends it with every later request, so it stays paired across restarts until
//! it is revoked from the machine. Only a hash of each token is stored.
//!
//! Requests from this machine's own pages need no token at all.

use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::store::{now_ms, write_atomic};

/// How long a code stays valid once it is shown.
pub const CODE_LIFETIME: Duration = Duration::from_secs(5 * 60);

/// Wrong guesses allowed for one code. After that the code is used up, and
/// the machine shows a new one.
pub const MAX_WRONG_GUESSES: u32 = 5;

const DEVICES_FILE: &str = "devices.json";
const USER_AGENT_LIMIT: usize = 120;

/// How a request was let through.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Access {
    /// A page on this machine, reached over its own loopback address.
    Local,
    /// A browser that paired with a code.
    Paired,
}

pub struct Pairing {
    file: PathBuf,
    state: Mutex<State>,
}

struct State {
    devices: Vec<Device>,
    /// The code on the machine's screen, if one is showing.
    code: Option<Code>,
}

struct Code {
    digits: String,
    expires: Instant,
    wrong: u32,
}

impl Code {
    fn new() -> Self {
        let n = Uuid::new_v4().as_u128() % 1_000_000;
        Self {
            digits: format!("{n:06}"),
            expires: Instant::now() + CODE_LIFETIME,
            wrong: 0,
        }
    }
}

#[derive(Clone, Serialize, Deserialize)]
struct Device {
    id: String,
    /// SHA-256 of the browser's token, in hex.
    token_hash: String,
    paired_at_ms: u64,
    user_agent: String,
}

/// What the machine's own screen shows about pairing.
#[derive(Serialize)]
pub struct PairingView {
    /// The code to enter on the other device, as two groups of three digits.
    pub code: String,
    pub expires_at_ms: u64,
    pub devices: Vec<DeviceView>,
}

#[derive(Serialize)]
pub struct DeviceView {
    pub id: String,
    pub paired_at_ms: u64,
    pub user_agent: String,
}

#[derive(Debug)]
pub enum RedeemError {
    /// No code is showing: it expired, was used up, or was already used.
    NoCode,
    /// The code is wrong. `left` guesses remain for this code.
    Wrong {
        left: u32,
    },
    Storage(anyhow::Error),
}

impl Pairing {
    /// Loads the paired browsers kept in `data_dir`.
    pub fn open(data_dir: &Path) -> Result<Self> {
        let file = data_dir.join(DEVICES_FILE);
        let devices = match std::fs::read(&file) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .with_context(|| format!("reading {}", file.display()))?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(e) => return Err(e).with_context(|| format!("reading {}", file.display())),
        };
        Ok(Self {
            file,
            state: Mutex::new(State {
                devices,
                code: None,
            }),
        })
    }

    /// The code showing on the machine, making one if none is showing.
    pub fn view(&self) -> PairingView {
        let mut st = self.lock();
        let code = st.code.get_or_insert_with(Code::new);
        if code.expires <= Instant::now() {
            *code = Code::new();
        }
        let remaining = code.expires.saturating_duration_since(Instant::now());
        let shown = format!("{} {}", &code.digits[..3], &code.digits[3..]);
        PairingView {
            code: shown,
            expires_at_ms: now_ms() + remaining.as_millis() as u64,
            devices: st.devices.iter().map(DeviceView::from).collect(),
        }
    }

    /// Replaces the code on the machine's screen, even if it has not expired.
    pub fn new_code(&self) {
        self.lock().code = Some(Code::new());
    }

    /// Checks a code typed on another device. A correct code is used up and
    /// returns a new token for that browser.
    pub fn redeem(&self, entered: &str, user_agent: &str) -> Result<String, RedeemError> {
        let typed: String = entered.chars().filter(char::is_ascii_digit).collect();
        let mut st = self.lock();
        let Some(mut code) = st.code.take().filter(|c| c.expires > Instant::now()) else {
            return Err(RedeemError::NoCode);
        };

        if same_text(&typed, &code.digits) {
            // The code stays taken: it works once.
            let token = new_token();
            let device = Device {
                id: Uuid::new_v4().simple().to_string(),
                token_hash: hash_token(&token),
                paired_at_ms: now_ms(),
                user_agent: user_agent.chars().take(USER_AGENT_LIMIT).collect(),
            };
            let mut devices = st.devices.clone();
            devices.push(device);
            save(&self.file, &devices).map_err(RedeemError::Storage)?;
            st.devices = devices;
            return Ok(token);
        }

        code.wrong += 1;
        let left = MAX_WRONG_GUESSES.saturating_sub(code.wrong);
        if left > 0 {
            st.code = Some(code);
        }
        Err(RedeemError::Wrong { left })
    }

    /// Whether `token` belongs to a paired browser.
    pub fn is_paired(&self, token: &str) -> bool {
        let hash = hash_token(token);
        self.lock()
            .devices
            .iter()
            .any(|d| same_text(&d.token_hash, &hash))
    }

    /// Forgets a paired browser, so its token stops working. Returns false if
    /// no browser has that id.
    pub fn revoke(&self, id: &str) -> Result<bool> {
        let mut st = self.lock();
        if !st.devices.iter().any(|d| d.id == id) {
            return Ok(false);
        }
        let devices: Vec<Device> = st.devices.iter().filter(|d| d.id != id).cloned().collect();
        save(&self.file, &devices)?;
        st.devices = devices;
        Ok(true)
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }
}

impl From<&Device> for DeviceView {
    fn from(d: &Device) -> Self {
        Self {
            id: d.id.clone(),
            paired_at_ms: d.paired_at_ms,
            user_agent: d.user_agent.clone(),
        }
    }
}

fn save(file: &Path, devices: &[Device]) -> Result<()> {
    write_atomic(file, &serde_json::to_vec_pretty(devices)?)
}

/// Two random UUIDs: 240 random bits, written as hex.
fn new_token() -> String {
    format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple())
}

fn hash_token(token: &str) -> String {
    Sha256::digest(token.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// Compares without stopping at the first difference, so a guess cannot be
/// refined by timing.
fn same_text(a: &str, b: &str) -> bool {
    a.len() == b.len()
        && a.bytes()
            .zip(b.bytes())
            .fold(0u8, |acc, (x, y)| acc | (x ^ y))
            == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn open() -> (tempfile::TempDir, Pairing) {
        let dir = tempfile::tempdir().unwrap();
        let pairing = Pairing::open(dir.path()).unwrap();
        (dir, pairing)
    }

    fn digits(pairing: &Pairing) -> String {
        pairing.view().code.replace(' ', "")
    }

    #[test]
    fn a_correct_code_pairs_once_and_its_token_is_accepted() {
        let (_dir, pairing) = open();
        let code = digits(&pairing);
        let token = pairing.redeem(&code, "phone").unwrap();
        assert!(pairing.is_paired(&token));
        assert!(!pairing.is_paired("not a token"));
        assert!(matches!(
            pairing.redeem(&code, "phone"),
            Err(RedeemError::NoCode)
        ));
    }

    #[test]
    fn a_code_may_be_typed_with_its_space_or_dashes() {
        let (_dir, pairing) = open();
        let shown = pairing.view().code;
        let dashed = shown.replace(' ', "-");
        assert!(pairing.redeem(&dashed, "tv").is_ok());
    }

    #[test]
    fn five_wrong_guesses_use_the_code_up() {
        let (_dir, pairing) = open();
        let code = digits(&pairing);
        let wrong = if code == "000000" { "111111" } else { "000000" };
        for left in (0..MAX_WRONG_GUESSES).rev() {
            match pairing.redeem(wrong, "x") {
                Err(RedeemError::Wrong { left: l }) => assert_eq!(l, left),
                other => panic!("unexpected {other:?}"),
            }
        }
        assert!(matches!(
            pairing.redeem(&code, "x"),
            Err(RedeemError::NoCode)
        ));
    }

    #[test]
    fn paired_browsers_survive_a_restart_and_can_be_revoked() {
        let dir = tempfile::tempdir().unwrap();
        let pairing = Pairing::open(dir.path()).unwrap();
        let token = pairing.redeem(&digits(&pairing), "phone").unwrap();
        let id = pairing.view().devices[0].id.clone();

        let stored = std::fs::read_to_string(dir.path().join(DEVICES_FILE)).unwrap();
        assert!(
            !stored.contains(&token),
            "the token itself must not be stored"
        );

        let reopened = Pairing::open(dir.path()).unwrap();
        assert!(reopened.is_paired(&token));
        assert!(reopened.revoke(&id).unwrap());
        assert!(!reopened.is_paired(&token));
        assert!(!Pairing::open(dir.path()).unwrap().is_paired(&token));
    }
}
