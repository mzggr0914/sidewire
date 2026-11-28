use crate::transport::MuxWriter;
use sidewire_protocol::FrameKind;
use std::{
    process::Stdio,
    sync::{Arc, Mutex, OnceLock, Weak},
    time::Duration,
};
use tokio::{
    process::{Child, Command},
    task::JoinHandle,
    time::MissedTickBehavior,
};

struct CpuLease {
    _child: Child,
}

fn acquire_cpu() -> Option<Arc<CpuLease>> {
    static OWNER: OnceLock<Mutex<Weak<CpuLease>>> = OnceLock::new();
    let mut owner = OWNER.get_or_init(Mutex::default).lock().unwrap();
    if let Some(lease) = owner.upgrade() {
        return Some(lease);
    }
    // Load privileged code only from beside the installed daemon. The copy
    // provisioned in shell-writable storage is for the unprivileged clipboard.
    let jar = std::env::current_exe()
        .ok()?
        .with_file_name("sidewire-clipboard.jar");
    if !jar.is_file() {
        tracing::warn!("shell wake helper is unavailable");
        return None;
    }
    match Command::new("/system/bin/app_process")
        .args(["/system/bin", "com.sidewire.ShellWakeLock"])
        .env("CLASSPATH", jar)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .kill_on_drop(true)
        .spawn()
    {
        Ok(child) => {
            let lease = Arc::new(CpuLease { _child: child });
            *owner = Arc::downgrade(&lease);
            Some(lease)
        }
        Err(error) => {
            tracing::warn!(%error, "could not start shell wake helper");
            None
        }
    }
}

pub(super) struct ShellActivity {
    _cpu: Option<Arc<CpuLease>>,
    pulse: JoinHandle<()>,
}

impl ShellActivity {
    pub(super) fn acquire(shared: &Mutex<Weak<Self>>, writer: MuxWriter) -> Arc<Self> {
        let mut lease = shared.lock().unwrap();
        if let Some(active) = lease.upgrade() {
            return active;
        }
        let cpu = acquire_cpu();
        let pulse = tokio::spawn(async move {
            let mut timer = tokio::time::interval(Duration::from_millis(100));
            timer.set_missed_tick_behavior(MissedTickBehavior::Skip);
            loop {
                timer.tick().await;
                if writer
                    .send_raw(FrameKind::ShellActivity, 0, &[])
                    .await
                    .is_err()
                {
                    break;
                }
            }
        });
        let active = Arc::new(Self { _cpu: cpu, pulse });
        *lease = Arc::downgrade(&active);
        active
    }
}
impl Drop for ShellActivity {
    fn drop(&mut self) {
        self.pulse.abort();
    }
}
