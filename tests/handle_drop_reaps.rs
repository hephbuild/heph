//! Regression: dropping a `proc_exec::Handle` without calling `wait*` must
//! still reap the child. Previously the kqueue watcher was only registered
//! inside `wait*`, so an error path between spawn and wait left the pid as a
//! permanent zombie (Drop's `kill_child` SIGKILLs but never `waitpid`s).

#![cfg(unix)]

use rheph::proc_exec;
use std::ffi::OsString;
use std::path::PathBuf;
use std::time::{Duration, Instant};

fn pid_in_proc_table(pid: i32) -> bool {
    // SAFETY: kill with sig=0 only probes existence. Zombies count as present;
    // reaped pids return ESRCH.
    let r = unsafe { libc::kill(pid, 0) };
    if r == 0 {
        return true;
    }
    let errno = std::io::Error::last_os_error().raw_os_error();
    errno != Some(libc::ESRCH)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dropped_handle_is_reaped_by_watcher() {
    let spec = proc_exec::Spec {
        program: PathBuf::from("sh"),
        args: vec![OsString::from("-c"), OsString::from("sleep 30")],
        env: Vec::new(),
        cwd: std::env::current_dir().expect("cwd"),
        stdin: proc_exec::StdioSpec::Null,
        stdout: proc_exec::StdioSpec::Null,
        stderr: proc_exec::StdioSpec::Null,
        setsid: false,
        ctty: false,
    };

    let handle = proc_exec::spawn(spec).expect("spawn");
    let pid = handle.pid();
    assert!(
        pid_in_proc_table(pid),
        "child must be live immediately after spawn"
    );

    // Simulate the error path: drop the Handle without consuming via wait*.
    // Drop sends SIGKILL via the supervisor; the watcher must then reap the
    // zombie via NOTE_EXIT or its 1s WNOHANG backstop.
    drop(handle);

    // Watcher's backstop polls every 1s; allow generous slack for CI jitter.
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if !pid_in_proc_table(pid) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("pid {pid} still in process table 5s after Handle drop — zombie leak");
}
