//! Identity, in two layers, with no account server and no personal data:
//!
//! - **Device key**: generated per device, wrapped in a self-signed
//!   certificate used for QUIC/TLS. `DeviceId` = BLAKE3 of that certificate.
//! - **Account key** (one per person): an Ed25519 key that signs each of the
//!   person's devices (`Attest`). Contacts are people, so they are keyed by
//!   `AccountId` = BLAKE3 of the account public key, and the short code people
//!   share (`7KQ3-M2XA-9FTB`) is the account's. Linking a new device copies the
//!   account key to it (with consent on both screens); a passphrase-encrypted
//!   backup can restore it.
//!
//! An attestation is public and only binds an account to a device id. TLS
//! proves the device id, so copying someone's attestation doesn't let an
//! attacker impersonate them.

use std::fmt;
use std::path::Path;

use anyhow::{anyhow, Context, Result};
use ring::signature::{Ed25519KeyPair, KeyPair, UnparsedPublicKey, ED25519};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use serde::{Deserialize, Serialize};

const CODE_ALPHABET: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";

macro_rules! id_type {
    ($name:ident) => {
        #[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
        pub struct $name(pub [u8; 32]);

        impl $name {
            /// Human-friendly short code (60 bits, Crockford base32): `XXXX-XXXX-XXXX`.
            pub fn code(&self) -> String {
                let mut bits: u64 = u64::from_be_bytes(self.0[..8].try_into().unwrap());
                let mut out = String::with_capacity(14);
                for i in 0..12 {
                    if i > 0 && i % 4 == 0 {
                        out.push('-');
                    }
                    out.push(CODE_ALPHABET[(bits >> 59) as usize] as char);
                    bits <<= 5;
                }
                out
            }

            pub fn to_hex(&self) -> String {
                self.0.iter().map(|b| format!("{b:02x}")).collect()
            }

            pub fn from_hex(s: &str) -> Option<Self> {
                Some($name(hex32(s)?))
            }
        }

        impl fmt::Debug for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "{}({})", stringify!($name), self.code())
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.code())
            }
        }

        impl Serialize for $name {
            fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
                s.serialize_str(&self.to_hex())
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
                let s = String::deserialize(d)?;
                $name::from_hex(&s).ok_or_else(|| serde::de::Error::custom("bad id"))
            }
        }
    };
}

id_type!(DeviceId);
id_type!(AccountId);

fn hex32(s: &str) -> Option<[u8; 32]> {
    if s.len() != 64 {
        return None;
    }
    let mut out = [0u8; 32];
    for (i, chunk) in s.as_bytes().chunks(2).enumerate() {
        out[i] = u8::from_str_radix(std::str::from_utf8(chunk).ok()?, 16).ok()?;
    }
    Some(out)
}

pub(crate) fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

pub(crate) fn unhex(s: &str) -> Option<Vec<u8>> {
    if s.len() % 2 != 0 {
        return None;
    }
    s.as_bytes().chunks(2).map(|c| u8::from_str_radix(std::str::from_utf8(c).ok()?, 16).ok()).collect()
}

/// Normalizes a typed code: drops separators/spaces, uppercases, maps the
/// look-alikes Crockford base32 allows (O→0, I/L→1).
pub fn normalize_code(input: &str) -> String {
    input
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .map(|c| match c.to_ascii_uppercase() {
            'O' => '0',
            'I' | 'L' => '1',
            c => c,
        })
        .collect()
}

impl DeviceId {
    pub fn of_cert(cert_der: &[u8]) -> Self {
        DeviceId(*blake3::hash(cert_der).as_bytes())
    }
}

impl AccountId {
    pub fn of_public(public: &[u8; 32]) -> Self {
        AccountId(*blake3::hash(public).as_bytes())
    }

    pub fn matches_code(&self, typed: &str) -> bool {
        let typed = normalize_code(typed);
        !typed.is_empty() && normalize_code(&self.code()) == typed
    }
}

/// "This device belongs to that account", signed by the account key.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Attest {
    /// Account public key (hex, 32 bytes).
    pub account: String,
    /// Ed25519 signature over `neo-device-v1` + device id (hex, 64 bytes).
    pub sig: String,
}

impl Attest {
    fn message(device: DeviceId) -> Vec<u8> {
        let mut m = b"neo-device-v1".to_vec();
        m.extend_from_slice(&device.0);
        m
    }

    /// Returns the account this attestation proves `device` belongs to.
    pub fn verify(&self, device: DeviceId) -> Option<AccountId> {
        let public: [u8; 32] = unhex(&self.account)?.try_into().ok()?;
        let sig = unhex(&self.sig)?;
        UnparsedPublicKey::new(&ED25519, public).verify(&Self::message(device), &sig).ok()?;
        Some(AccountId::of_public(&public))
    }
}

/// The person's account key (Ed25519, PKCS#8).
pub struct Account {
    pkcs8: Vec<u8>,
    pair: Ed25519KeyPair,
}

impl Clone for Account {
    fn clone(&self) -> Self {
        Account::from_pkcs8(self.pkcs8.clone()).expect("valid key")
    }
}

impl Account {
    pub fn generate() -> Result<Self> {
        let rng = ring::rand::SystemRandom::new();
        let pkcs8 = Ed25519KeyPair::generate_pkcs8(&rng).map_err(|_| anyhow!("key generation failed"))?;
        Self::from_pkcs8(pkcs8.as_ref().to_vec())
    }

    pub fn from_pkcs8(pkcs8: Vec<u8>) -> Result<Self> {
        let pair = Ed25519KeyPair::from_pkcs8(&pkcs8).map_err(|_| anyhow!("invalid account key"))?;
        Ok(Account { pkcs8, pair })
    }

    pub fn pkcs8(&self) -> &[u8] {
        &self.pkcs8
    }

    /// The 32-byte Ed25519 seed inside the PKCS#8 document (for signing DHT
    /// records with the same key). ring writes it right after the
    /// `OCTET STRING(OCTET STRING(32))` header at offset 16.
    pub fn seed(&self) -> Option<[u8; 32]> {
        let p = &self.pkcs8;
        (p.len() >= 48 && p[12..16] == [0x04, 0x22, 0x04, 0x20]).then(|| p[16..48].try_into().unwrap())
    }

    pub fn public(&self) -> [u8; 32] {
        self.pair.public_key().as_ref().try_into().unwrap()
    }

    pub fn id(&self) -> AccountId {
        AccountId::of_public(&self.public())
    }

    pub fn attest(&self, device: DeviceId) -> Attest {
        let sig = self.pair.sign(&Attest::message(device));
        Attest { account: hex(&self.public()), sig: hex(sig.as_ref()) }
    }
}

pub struct Identity {
    pub cert: CertificateDer<'static>,
    key_der: Vec<u8>,
    pub id: DeviceId,
}

impl Identity {
    /// Loads the device identity stored in `dir`, creating one on first run.
    pub fn load_or_create(dir: &Path) -> Result<Self> {
        let cert_path = dir.join("identity.cert.der");
        let key_path = dir.join("identity.key.der");
        if let (Ok(cert), Ok(key)) = (std::fs::read(&cert_path), std::fs::read(&key_path)) {
            return Ok(Self::from_parts(cert, key));
        }
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        let key = rcgen::KeyPair::generate_for(&rcgen::PKCS_ED25519)?;
        let params = rcgen::CertificateParams::new(vec!["neo".to_string()])?;
        let cert = params.self_signed(&key)?;
        let cert_der = cert.der().to_vec();
        let key_der = key.serialize_der();
        std::fs::write(&key_path, &key_der)?;
        std::fs::write(&cert_path, &cert_der)?;
        Ok(Self::from_parts(cert_der, key_der))
    }

    fn from_parts(cert: Vec<u8>, key_der: Vec<u8>) -> Self {
        let id = DeviceId::of_cert(&cert);
        Identity { cert: CertificateDer::from(cert), key_der, id }
    }

    pub fn key(&self) -> PrivateKeyDer<'static> {
        PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(self.key_der.clone()))
    }
}

/// Loads the account key from `dir`, creating one on first run.
pub fn load_or_create_account(dir: &Path) -> Result<Account> {
    let path = dir.join("account.key.der");
    if let Ok(bytes) = std::fs::read(&path) {
        return Account::from_pkcs8(bytes);
    }
    let account = Account::generate()?;
    save_account(dir, &account)?;
    Ok(account)
}

pub fn save_account(dir: &Path, account: &Account) -> Result<()> {
    std::fs::create_dir_all(dir)?;
    let tmp = dir.join("account.key.der.tmp");
    std::fs::write(&tmp, account.pkcs8())?;
    std::fs::rename(tmp, dir.join("account.key.der"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn code_and_hex_roundtrip() {
        let id = DeviceId::of_cert(b"hello");
        let code = id.code();
        assert_eq!(code.len(), 14);
        assert_eq!(code.matches('-').count(), 2);
        assert_eq!(DeviceId::from_hex(&id.to_hex()), Some(id));
    }

    #[test]
    fn attestations_bind_account_to_device() {
        let account = Account::generate().unwrap();
        let device = DeviceId::of_cert(b"device-a");
        let attest = account.attest(device);
        assert_eq!(attest.verify(device), Some(account.id()));
        // Replaying it for another device fails.
        assert_eq!(attest.verify(DeviceId::of_cert(b"device-b")), None);
        // So does someone else's key.
        let other = Account::generate().unwrap();
        let forged = Attest { account: attest.account.clone(), sig: other.attest(device).sig };
        assert_eq!(forged.verify(device), None);
    }

    #[test]
    fn seed_matches_the_public_key() {
        let account = Account::generate().unwrap();
        let seed = account.seed().expect("seed");
        let dalek = mainline::SigningKey::from_bytes(&seed);
        assert_eq!(dalek.verifying_key().to_bytes(), account.public());
    }

    #[test]
    fn typed_codes_are_forgiving() {
        let id = AccountId::of_public(&[7; 32]);
        let code = id.code();
        assert!(id.matches_code(&code.to_lowercase().replace('-', " ")));
        assert!(!id.matches_code(""));
    }
}
