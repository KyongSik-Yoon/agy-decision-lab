//! 훅을 띄운 agy 프로세스의 실행 파일 찾기.
//!
//! 부모부터 조상 프로세스를 최대 6단계 올라가며, 실행 파일 이름이 `antigravity` 로 시작하는
//! 첫 프로세스의 실행 파일 경로를 돌려준다. 셸에서 직접 돌리는 등 찾지 못하면 `None`.

use std::path::PathBuf;

const MAX_DEPTH: usize = 6;

pub fn find_agy_ancestor() -> Option<PathBuf> {
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    {
        let mut pid = std::os::unix::process::parent_id();
        for _ in 0..MAX_DEPTH {
            if pid <= 1 {
                return None;
            }
            if let Some((open_path, target)) = process_exe(pid)
                && target
                    .file_name()
                    .is_some_and(|n| n.to_string_lossy().starts_with("antigravity"))
            {
                return Some(open_path);
            }
            pid = parent_pid(pid)?;
        }
    }
    None
}

/// (열 경로, 이름을 볼 실제 파일 경로)
#[cfg(target_os = "linux")]
fn process_exe(pid: u32) -> Option<(PathBuf, PathBuf)> {
    // 링크 자체를 연다: 실행 중에 업데이트로 파일이 바뀌거나 지워져도 그 프로세스가 돌리는 바이너리를 읽는다
    let exe = PathBuf::from(format!("/proc/{pid}/exe"));
    let target = std::fs::read_link(&exe).ok()?;
    Some((exe, target))
}

#[cfg(target_os = "linux")]
fn parent_pid(pid: u32) -> Option<u32> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // "pid (comm) state ppid ..." — comm 에 공백·괄호가 있을 수 있어 마지막 ')' 뒤를 본다
    let rest = stat.get(stat.rfind(')')? + 1..)?;
    rest.split_whitespace().nth(1)?.parse().ok()
}

/// macOS 에는 `/proc` 이 없어 libproc 으로 경로만 얻는다.
/// 실행 중에 agy 가 업데이트돼 파일이 바뀌면 그 자리의 새 파일을 읽게 된다.
#[cfg(target_os = "macos")]
fn process_exe(pid: u32) -> Option<(PathBuf, PathBuf)> {
    use std::os::unix::ffi::OsStringExt;
    let mut buf = vec![0u8; libc::PROC_PIDPATHINFO_MAXSIZE as usize];
    // SAFETY: buf 는 넘기는 길이만큼 쓸 수 있는 버퍼다
    let n = unsafe {
        libc::proc_pidpath(
            pid as libc::c_int,
            buf.as_mut_ptr().cast(),
            buf.len() as u32,
        )
    };
    if n <= 0 {
        return None;
    }
    buf.truncate(n as usize);
    let path = PathBuf::from(std::ffi::OsString::from_vec(buf));
    Some((path.clone(), path))
}

#[cfg(target_os = "macos")]
fn parent_pid(pid: u32) -> Option<u32> {
    // SAFETY: proc_bsdinfo 는 정수·바이트 배열뿐인 C 구조체라 0 으로 채워도 유효하다
    let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of::<libc::proc_bsdinfo>() as libc::c_int;
    // SAFETY: info 는 size 바이트짜리 쓰기 가능한 proc_bsdinfo 다
    let n = unsafe {
        libc::proc_pidinfo(
            pid as libc::c_int,
            libc::PROC_PIDTBSDINFO,
            0,
            (&mut info as *mut libc::proc_bsdinfo).cast(),
            size,
        )
    };
    (n == size).then_some(info.pbi_ppid)
}
