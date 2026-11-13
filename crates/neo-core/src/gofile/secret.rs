//! Keeps the Gofile API token encrypted at rest with Windows DPAPI
//! (`CryptProtectData`): only this Windows user on this PC can decrypt it.
//! The plaintext token is never written to disk.

use std::path::Path;

use anyhow::{anyhow, Result};

const FILE: &str = "gofile.token";
/// Extra entropy so other apps' DPAPI blobs can't be swapped in.
const ENTROPY: &[u8] = b"neo-gofile-token-v1";

pub fn save(dir: &Path, token: &str) -> Result<()> {
    let blob = protect(token.as_bytes())?;
    std::fs::create_dir_all(dir)?;
    let tmp = dir.join("gofile.token.tmp");
    std::fs::write(&tmp, blob)?;
    std::fs::rename(tmp, dir.join(FILE))?;
    Ok(())
}

pub fn load(dir: &Path) -> Option<String> {
    let blob = std::fs::read(dir.join(FILE)).ok()?;
    String::from_utf8(unprotect(&blob).ok()?).ok()
}

pub fn forget(dir: &Path) {
    let _ = std::fs::remove_file(dir.join(FILE));
}

#[cfg(windows)]
fn protect(plain: &[u8]) -> Result<Vec<u8>> {
    dpapi(plain, true)
}

#[cfg(windows)]
fn unprotect(blob: &[u8]) -> Result<Vec<u8>> {
    dpapi(blob, false)
}

#[cfg(windows)]
fn dpapi(input: &[u8], encrypt: bool) -> Result<Vec<u8>> {
    use windows_sys::Win32::Foundation::LocalFree;
    use windows_sys::Win32::Security::Cryptography::{
        CryptProtectData, CryptUnprotectData, CRYPTPROTECT_UI_FORBIDDEN, CRYPT_INTEGER_BLOB,
    };
    let data_in = CRYPT_INTEGER_BLOB { cbData: input.len() as u32, pbData: input.as_ptr() as *mut u8 };
    let entropy = CRYPT_INTEGER_BLOB { cbData: ENTROPY.len() as u32, pbData: ENTROPY.as_ptr() as *mut u8 };
    let mut out = CRYPT_INTEGER_BLOB { cbData: 0, pbData: std::ptr::null_mut() };
    let ok = unsafe {
        if encrypt {
            CryptProtectData(
                &data_in,
                std::ptr::null(),
                &entropy,
                std::ptr::null(),
                std::ptr::null(),
                CRYPTPROTECT_UI_FORBIDDEN,
                &mut out,
            )
        } else {
            CryptUnprotectData(
                &data_in,
                std::ptr::null_mut(),
                &entropy,
                std::ptr::null(),
                std::ptr::null(),
                CRYPTPROTECT_UI_FORBIDDEN,
                &mut out,
            )
        }
    };
    if ok == 0 || out.pbData.is_null() {
        return Err(anyhow!("Windows couldn't {} the token", if encrypt { "protect" } else { "unlock" }));
    }
    let result = unsafe { std::slice::from_raw_parts(out.pbData, out.cbData as usize).to_vec() };
    unsafe {
        LocalFree(out.pbData as _);
    }
    Ok(result)
}

#[cfg(not(windows))]
fn protect(_plain: &[u8]) -> Result<Vec<u8>> {
    Err(anyhow!("secure token storage is only implemented on Windows"))
}

#[cfg(not(windows))]
fn unprotect(_blob: &[u8]) -> Result<Vec<u8>> {
    Err(anyhow!("secure token storage is only implemented on Windows"))
}

#[cfg(all(test, windows))]
mod tests {
    #[test]
    fn token_roundtrips_and_is_not_plaintext_on_disk() {
        let dir = std::env::temp_dir().join(format!("neo-gofile-secret-{}", std::process::id()));
        super::save(&dir, "TOKEN-abc123").unwrap();
        let raw = std::fs::read(dir.join(super::FILE)).unwrap();
        assert!(!raw.windows(6).any(|w| w == b"abc123"));
        assert_eq!(super::load(&dir).as_deref(), Some("TOKEN-abc123"));
        super::forget(&dir);
        assert_eq!(super::load(&dir), None);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
