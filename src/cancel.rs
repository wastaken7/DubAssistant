use crate::media::configure_no_window;
use anyhow::{bail, Context, Result};
use std::io::Read;
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

#[derive(Clone, Default)]
pub struct CancelHandle {
    cancelled: Arc<AtomicBool>,
    active_pid: Arc<Mutex<Option<u32>>>,
    async_abort: Arc<Mutex<Option<tokio::task::AbortHandle>>>,
}

impl CancelHandle {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn reset(&self) {
        self.cancelled.store(false, Ordering::SeqCst);
        if let Ok(mut pid) = self.active_pid.lock() {
            *pid = None;
        }
        if let Ok(mut abort) = self.async_abort.lock() {
            *abort = None;
        }
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::SeqCst)
    }

    pub fn set_async_abort(&self, handle: Option<tokio::task::AbortHandle>) {
        if let Ok(mut guard) = self.async_abort.lock() {
            *guard = handle;
        }
    }

    /// Cancela imediatamente a tarefa atual, abortando futures async e encerrando qualquer processo filho.
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::SeqCst);

        if let Ok(mut guard) = self.async_abort.lock() {
            if let Some(h) = guard.take() {
                h.abort();
            }
        }

        if let Ok(mut guard) = self.active_pid.lock() {
            if let Some(pid) = guard.take() {
                kill_process_tree(pid);
            }
        }
    }

    fn set_pid(&self, pid: Option<u32>) {
        if let Ok(mut guard) = self.active_pid.lock() {
            *guard = pid;
        }
    }
}

fn kill_process_tree(pid: u32) {
    #[cfg(target_os = "windows")]
    {
        let mut cmd = Command::new("taskkill");
        configure_no_window(&mut cmd);
        let _ = cmd
            .args(["/F", "/T", "/PID", &pid.to_string()])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = Command::new("kill")
            .args(["-9", &pid.to_string()])
            .status();
    }
}

/// Executa um comando externo de forma cancelável, lendo stdout/stderr sem deadlock
/// e encerrando o processo imediatamente se `cancel` for acionado.
pub fn run_command_cancellable(cmd: &mut Command, cancel: &CancelHandle) -> Result<Output> {
    if cancel.is_cancelled() {
        bail!("Tarefa cancelada pelo usuário.");
    }

    configure_no_window(cmd);
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());

    let mut child = cmd.spawn().context("Falha ao iniciar processo externo")?;
    let pid = child.id();
    cancel.set_pid(Some(pid));

    let mut stdout_pipe = child.stdout.take();
    let mut stderr_pipe = child.stderr.take();

    let out_thread = std::thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(ref mut s) = stdout_pipe {
            let _ = s.read_to_end(&mut buf);
        }
        buf
    });
    let err_thread = std::thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(ref mut s) = stderr_pipe {
            let _ = s.read_to_end(&mut buf);
        }
        buf
    });

    let status = loop {
        if cancel.is_cancelled() {
            let _ = child.kill();
            let _ = child.wait();
            cancel.set_pid(None);
            let _ = out_thread.join();
            let _ = err_thread.join();
            bail!("Tarefa cancelada pelo usuário.");
        }

        match child.try_wait()? {
            Some(st) => break st,
            None => std::thread::sleep(Duration::from_millis(50)),
        }
    };

    cancel.set_pid(None);
    let stdout = out_thread.join().unwrap_or_default();
    let stderr = err_thread.join().unwrap_or_default();

    Ok(Output {
        status,
        stdout,
        stderr,
    })
}
