//! DPAPI(CurrentUser)로 봉인한 파일 저장소입니다.
//!
//! 키 파일을 따로 두지 않고 내용 전체를 `CryptProtectData`로 감쌉니다. 같은 Windows
//! 계정에서만 풀리므로, 다른 계정이거나 파일이 손상됐으면 읽기 실패로 돌려주고
//! 호출 쪽이 "로그인 필요"로 처리합니다. 쓰기는 임시 파일 + rename으로 원자적으로 합니다.

use std::io;
use std::path::Path;

use windows_sys::Win32::Foundation::LocalFree;
use windows_sys::Win32::Security::Cryptography::{
    CryptProtectData, CryptUnprotectData, CRYPTPROTECT_UI_FORBIDDEN, CRYPT_INTEGER_BLOB,
};

fn blob_of(data: &[u8]) -> CRYPT_INTEGER_BLOB {
    CRYPT_INTEGER_BLOB {
        cbData: data.len() as u32,
        pbData: data.as_ptr() as *mut u8,
    }
}

/// DPAPI가 LocalAlloc으로 내준 버퍼를 복사한 뒤 바로 돌려줍니다.
fn take_output(out: CRYPT_INTEGER_BLOB) -> Vec<u8> {
    if out.pbData.is_null() {
        return Vec::new();
    }
    // SAFETY: 성공한 DPAPI 호출이 cbData 바이트짜리 버퍼를 채워 준 경우만 여기로 옵니다.
    let bytes = unsafe { std::slice::from_raw_parts(out.pbData, out.cbData as usize) }.to_vec();
    // SAFETY: DPAPI 출력 버퍼는 LocalFree로 해제하도록 문서화돼 있습니다.
    unsafe { LocalFree(out.pbData.cast()) };
    bytes
}

pub fn seal(plain: &[u8]) -> io::Result<Vec<u8>> {
    let input = blob_of(plain);
    let mut out = CRYPT_INTEGER_BLOB::default();
    // SAFETY: 입력 블롭은 이 함수가 끝날 때까지 살아 있는 슬라이스를 가리킵니다.
    let ok = unsafe {
        CryptProtectData(
            &input,
            std::ptr::null(),
            std::ptr::null(),
            std::ptr::null(),
            std::ptr::null(),
            CRYPTPROTECT_UI_FORBIDDEN,
            &mut out,
        )
    };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(take_output(out))
}

pub fn unseal(sealed: &[u8]) -> io::Result<Vec<u8>> {
    let input = blob_of(sealed);
    let mut out = CRYPT_INTEGER_BLOB::default();
    // SAFETY: seal과 같습니다. 설명 문자열은 받지 않습니다(null).
    let ok = unsafe {
        CryptUnprotectData(
            &input,
            std::ptr::null_mut(),
            std::ptr::null(),
            std::ptr::null(),
            std::ptr::null(),
            CRYPTPROTECT_UI_FORBIDDEN,
            &mut out,
        )
    };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(take_output(out))
}

/// 봉인해서 원자적으로 씁니다. 중간에 실패해도 이전 파일은 그대로 남습니다.
pub fn write(path: &Path, plain: &[u8]) -> io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let sealed = seal(plain)?;
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    let tmp = std::path::PathBuf::from(tmp);
    std::fs::write(&tmp, sealed)?;
    // Windows에서 std::fs::rename은 MOVEFILE_REPLACE_EXISTING이라 기존 파일을 교체합니다.
    std::fs::rename(&tmp, path).inspect_err(|_| {
        let _ = std::fs::remove_file(&tmp);
    })
}

/// 파일이 없으면 `Ok(None)`. 풀리지 않으면(다른 계정·손상) 오류입니다.
pub fn read(path: &Path) -> io::Result<Option<Vec<u8>>> {
    match std::fs::read(path) {
        Ok(sealed) => unseal(&sealed).map(Some),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

pub fn remove(path: &Path) -> io::Result<()> {
    match std::fs::remove_file(path) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("cuw-secure-store-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn dpapi_round_trip() {
        let plain = br#"{"cookies":[{"name":"sessionKey","value":"x"}]}"#;
        let sealed = seal(plain).unwrap();
        assert_ne!(sealed.as_slice(), plain.as_slice());
        assert_eq!(unseal(&sealed).unwrap(), plain);
    }

    #[test]
    fn unseal_rejects_garbage() {
        assert!(unseal(b"not a dpapi blob").is_err());
    }

    #[test]
    fn write_read_replace_remove() {
        let dir = temp_dir("rw");
        let path = dir.join("session.dat");
        assert!(read(&path).unwrap().is_none());

        write(&path, b"first").unwrap();
        write(&path, b"second").unwrap();
        assert_eq!(read(&path).unwrap().as_deref(), Some(&b"second"[..]));
        assert!(!dir.join("session.dat.tmp").exists());
        // 디스크에는 평문이 없어야 합니다.
        assert!(!std::fs::read(&path)
            .unwrap()
            .windows(6)
            .any(|w| w == b"second"));

        remove(&path).unwrap();
        remove(&path).unwrap();
        assert!(read(&path).unwrap().is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn corrupt_file_is_an_error_not_empty() {
        let dir = temp_dir("corrupt");
        let path = dir.join("session.dat");
        std::fs::write(&path, b"garbage").unwrap();
        assert!(read(&path).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
