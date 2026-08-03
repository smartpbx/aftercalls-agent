//! Small, synchronous child-process ownership primitives for local media work.
//!
//! Media children are different from ordinary "run a command" helpers:
//! callers must keep draining their pipes, must be able to kill the exact
//! in-flight child from a Stop timeout, and must reap it before returning.
//! `ChildSlot` is intentionally process-local; durable state lives in
//! `media_manifest`.

use anyhow::{anyhow, Context, Result};
use std::io::Read;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

pub const STDERR_LIMIT_BYTES: usize = 64 * 1024;

#[derive(Clone, Default)]
pub struct ChildSlot {
    inner: Arc<Mutex<Option<Child>>>,
    reaped: Arc<AtomicBool>,
}

#[derive(Debug)]
pub enum ProcessTermination {
    Exited(ExitStatus),
    TimedOut,
    Cancelled,
    WaitFailed(String),
}

#[derive(Debug)]
pub struct BoundedProcessOutput {
    pub termination: ProcessTermination,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub stderr_truncated: bool,
    /// True when no child remains in the slot. Timeout/cancellation paths set
    /// this only after `kill` followed by `wait`.
    #[cfg_attr(not(test), allow(dead_code))]
    pub reaped: bool,
}

impl BoundedProcessOutput {
    pub fn success(&self) -> bool {
        matches!(&self.termination, ProcessTermination::Exited(status) if status.success())
    }

    pub fn diagnostic(&self) -> String {
        let text = String::from_utf8_lossy(&self.stderr).trim().to_string();
        if text.is_empty() {
            match &self.termination {
                ProcessTermination::WaitFailed(error) => error.clone(),
                other => format!("{other:?}"),
            }
        } else if self.stderr_truncated {
            format!("{text} [truncated]")
        } else {
            text
        }
    }
}

impl ChildSlot {
    pub fn install(&self, child: Child) -> std::result::Result<(), Child> {
        let mut guard = self.inner.lock().unwrap();
        if guard.is_some() {
            return Err(child);
        }
        self.reaped.store(false, Ordering::SeqCst);
        *guard = Some(child);
        Ok(())
    }

    /// Wait for the installed child without ever abandoning it. On timeout or
    /// cancellation the child is killed and reaped before this returns.
    pub fn wait(&self, timeout: Duration, cancel: Option<&AtomicBool>) -> ProcessTermination {
        let deadline = Instant::now() + timeout;
        loop {
            if cancel
                .map(|flag| flag.load(Ordering::SeqCst))
                .unwrap_or(false)
            {
                let _ = self.kill_and_reap();
                return ProcessTermination::Cancelled;
            }

            let polled = {
                let mut guard = self.inner.lock().unwrap();
                match guard.as_mut() {
                    Some(child) => match child.try_wait() {
                        Ok(Some(status)) => {
                            // `try_wait(Some)` has collected the exit status;
                            // dropping the handle cannot leave a zombie.
                            self.reaped.store(true, Ordering::SeqCst);
                            *guard = None;
                            Some(Ok(status))
                        }
                        Ok(None) => None,
                        Err(e) => Some(Err(e)),
                    },
                    None => {
                        return ProcessTermination::WaitFailed(
                            "media child disappeared before exit was observed".into(),
                        );
                    }
                }
            };

            match polled {
                Some(Ok(status)) => return ProcessTermination::Exited(status),
                Some(Err(e)) => {
                    let _ = self.kill_and_reap();
                    return ProcessTermination::WaitFailed(e.to_string());
                }
                None if Instant::now() >= deadline => {
                    let _ = self.kill_and_reap();
                    return ProcessTermination::TimedOut;
                }
                None => thread::sleep(Duration::from_millis(25)),
            }
        }
    }

    /// Kill and collect the installed child. Returns whether a child was
    /// present and `wait` completed. Calling this repeatedly is safe.
    pub fn kill_and_reap(&self) -> bool {
        let child = self.inner.lock().unwrap().take();
        let Some(mut child) = child else {
            return true;
        };
        let _ = child.kill();
        let reaped = child.wait().is_ok();
        self.reaped.store(reaped, Ordering::SeqCst);
        reaped
    }

    pub fn was_reaped(&self) -> bool {
        self.reaped.load(Ordering::SeqCst)
    }
}

struct Drained {
    bytes: Vec<u8>,
    truncated: bool,
}

fn spawn_bounded_drain<R>(mut reader: R, cap: usize, retain_tail: bool) -> JoinHandle<Drained>
where
    R: Read + Send + 'static,
{
    thread::spawn(move || {
        let mut kept = Vec::with_capacity(cap.min(8192));
        let mut buf = [0u8; 8192];
        let mut truncated = false;
        loop {
            match reader.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    if retain_tail {
                        if n >= cap {
                            kept.clear();
                            kept.extend_from_slice(&buf[n - cap..n]);
                            truncated = true;
                        } else {
                            let overflow = kept.len().saturating_add(n).saturating_sub(cap);
                            if overflow > 0 {
                                kept.drain(..overflow);
                                truncated = true;
                            }
                            kept.extend_from_slice(&buf[..n]);
                        }
                    } else {
                        let room = cap.saturating_sub(kept.len());
                        let take = room.min(n);
                        kept.extend_from_slice(&buf[..take]);
                        if take < n {
                            truncated = true;
                        }
                    }
                }
                Err(_) => break,
            }
        }
        Drained {
            bytes: kept,
            truncated,
        }
    })
}

/// Run a command with bounded stdout/stderr capture. `slot` makes the exact
/// child killable by an owner outside this function (the rolling Stop path).
pub fn run_bounded_in_slot(
    mut command: Command,
    timeout: Duration,
    output_cap: usize,
    slot: &ChildSlot,
    cancel: Option<&AtomicBool>,
) -> Result<BoundedProcessOutput> {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    no_console_std(&mut command);
    let mut child = command.spawn().context("spawn media child")?;
    let stdout = match child.stdout.take() {
        Some(stdout) => stdout,
        None => {
            let _ = child.kill();
            let _ = child.wait();
            return Err(anyhow!("media child stdout was not piped"));
        }
    };
    let stderr = match child.stderr.take() {
        Some(stderr) => stderr,
        None => {
            drop(stdout);
            let _ = child.kill();
            let _ = child.wait();
            return Err(anyhow!("media child stderr was not piped"));
        }
    };
    // ffmpeg progress completion and final duration are at the tail. stderr
    // keeps its prefix, where ffmpeg normally prints the root error.
    let stdout_join = spawn_bounded_drain(stdout, output_cap, true);
    let stderr_join = spawn_bounded_drain(stderr, output_cap, false);

    if let Err(mut child) = slot.install(child) {
        // The child is still owned by `child` only until install; on an
        // impossible occupied-slot error, make sure its pipe readers unblock.
        let _ = child.kill();
        let _ = child.wait();
        let _ = stdout_join.join();
        let _ = stderr_join.join();
        return Err(anyhow!("media child slot already occupied"));
    }

    let termination = slot.wait(timeout, cancel);
    let reaped = slot.was_reaped();
    let stdout = stdout_join
        .join()
        .map_err(|_| anyhow!("media stdout drain panicked"))?;
    let stderr = stderr_join
        .join()
        .map_err(|_| anyhow!("media stderr drain panicked"))?;
    Ok(BoundedProcessOutput {
        termination,
        stdout: stdout.bytes,
        stderr: stderr.bytes,
        stderr_truncated: stderr.truncated,
        reaped,
    })
}

pub fn run_bounded(
    command: Command,
    timeout: Duration,
    output_cap: usize,
) -> Result<BoundedProcessOutput> {
    run_bounded_in_slot(command, timeout, output_cap, &ChildSlot::default(), None)
}

/// Suppress the transient Windows console window for every synchronous media
/// child. No-op on other platforms.
#[cfg(windows)]
pub fn no_console_std(cmd: &mut Command) {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    cmd.creation_flags(CREATE_NO_WINDOW);
}

#[cfg(not(windows))]
pub fn no_console_std(_cmd: &mut Command) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn timeout_kills_and_reaps_child() {
        let mut cmd = Command::new("/bin/sleep");
        cmd.arg("30");
        let out =
            run_bounded(cmd, Duration::from_millis(75), 1024).expect("bounded child should run");
        assert!(matches!(out.termination, ProcessTermination::TimedOut));
        assert!(out.reaped, "timed-out process must be reaped");
    }

    #[cfg(unix)]
    #[test]
    fn stderr_is_drained_but_bounded() {
        let mut cmd = Command::new("/bin/sh");
        cmd.arg("-c")
            .arg("i=0; while [ $i -lt 5000 ]; do printf 0123456789 >&2; i=$((i+1)); done");
        let out =
            run_bounded(cmd, Duration::from_secs(2), 1024).expect("stderr writer should finish");
        assert!(out.success());
        assert_eq!(out.stderr.len(), 1024);
        assert!(out.stderr_truncated);
    }
}
