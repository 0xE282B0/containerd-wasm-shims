use std::{
    fs::File,
    io::{ErrorKind, Read},
    path::{Path, PathBuf},
    sync::{Arc, Condvar, Mutex},
    thread,
};

use chrono::{DateTime, Utc};
use containerd_shim::{error, run};
use containerd_shim_wasm::sandbox::{
    instance::Wait,
    instance_utils::{get_instance_root, instance_exists, maybe_open_stdio},
    EngineGetter, Error, Instance, InstanceConfig, ShimCli,
};
use libc::{SIGINT, SIGKILL};
use libcontainer::{
    container::{builder::ContainerBuilder, Container, ContainerStatus},
    signal::Signal,
    syscall::syscall::create_syscall,
};
use nix::errno::Errno;
use nix::sys::wait::{waitid, Id as WaitID, WaitPidFlag, WaitStatus};
use serde::{Deserialize, Serialize};

use anyhow::{Context, Result};

use crate::executor::LunaticExecutor;

type ExitCode = Arc<(Mutex<Option<(u32, DateTime<Utc>)>>, Condvar)>;

mod common;
mod executor;

static DEFAULT_CONTAINER_ROOT_DIR: &str = "/run/containerd/lunatic";

pub struct Wasi {
    exit_code: ExitCode,
    bundle: String,
    rootdir: PathBuf,
    id: String,
    stdin: String,
    stdout: String,
    stderr: String,
}

#[derive(Serialize, Deserialize)]
struct Options {
    root: Option<PathBuf>,
}

fn determine_rootdir<P: AsRef<Path>>(bundle: P, namespace: String) -> Result<PathBuf, Error> {
    let mut file = match File::open(bundle.as_ref().join("options.json")) {
        Ok(f) => f,
        Err(err) => match err.kind() {
            ErrorKind::NotFound => {
                return Ok(<&str as Into<PathBuf>>::into(DEFAULT_CONTAINER_ROOT_DIR).join(namespace))
            }
            _ => return Err(err.into()),
        },
    };
    let mut data = String::new();
    file.read_to_string(&mut data)?;
    let options: Options = serde_json::from_str(&data)?;
    Ok(options
        .root
        .unwrap_or(PathBuf::from(DEFAULT_CONTAINER_ROOT_DIR))
        .join(namespace))
}

impl Instance for Wasi {
    type E = ();
    fn new(id: String, cfg: Option<&InstanceConfig<Self::E>>) -> Self {
        let cfg = cfg.unwrap();
        let bundle = cfg.get_bundle().unwrap_or_default();
        let rootdir = determine_rootdir(bundle.as_str(), cfg.get_namespace()).unwrap();
        Wasi {
            id,
            exit_code: Arc::new((Mutex::new(None), Condvar::new())),
            bundle,
            rootdir,
            stdin: cfg.get_stdin().unwrap_or_default(),
            stdout: cfg.get_stdout().unwrap_or_default(),
            stderr: cfg.get_stderr().unwrap_or_default(),
        }
    }

    fn start(&self) -> Result<u32, Error> {
        log::info!("starting instance: {}", self.id);
        let mut container = self.build_container()?;
        log::info!("created container: {}", self.id);
        let code = self.exit_code.clone();
        let pid = container.pid().unwrap();

        container
            .start()
            .map_err(|err| Error::Any(anyhow::anyhow!("failed to start container: {}", err)))?;
        thread::spawn(move || {
            let (lock, cvar) = &*code;

            let status = match waitid(WaitID::Pid(pid), WaitPidFlag::WEXITED) {
                Ok(WaitStatus::Exited(_, status)) => status,
                Ok(WaitStatus::Signaled(_, sig, _)) => sig as i32,
                Ok(_) => 0,
                Err(e) => {
                    if e == Errno::ECHILD {
                        log::info!("no child process");
                        0
                    } else {
                        panic!("waitpid failed: {}", e);
                    }
                }
            } as u32;
            let mut ec = lock.lock().unwrap();
            *ec = Some((status, Utc::now()));
            drop(ec);
            cvar.notify_all();
        });

        Ok(pid.as_raw() as u32)
    }

    fn kill(&self, signal: u32) -> Result<(), Error> {
        log::info!("killing instance: {}", self.id);
        if signal as i32 != SIGKILL && signal as i32 != SIGINT {
            return Err(Error::InvalidArgument(
                "only SIGKILL and SIGINT are supported".to_string(),
            ));
        }
        let container_root = get_instance_root(&self.rootdir, self.id.as_str())?;
        let mut container = Container::load(container_root).with_context(|| {
            format!(
                "could not load state for container {id}",
                id = self.id.as_str()
            )
        })?;
        let signal = Signal::try_from(signal as i32)
            .map_err(|err| Error::InvalidArgument(format!("invalid signal number: {}", err)))?;
        match container.kill(signal, true) {
            Ok(_) => Ok(()),
            Err(e) => {
                if container.status() == ContainerStatus::Stopped {
                    return Err(Error::Others("container not running".into()));
                }
                Err(Error::Others(e.to_string()))
            }
        }
    }

    fn delete(&self) -> Result<(), Error> {
        log::info!("deleting instance: {}", self.id);
        match instance_exists(&self.rootdir, self.id.as_str()) {
            Ok(exists) => {
                if !exists {
                    return Ok(());
                }
            }
            Err(err) => {
                error!("could not find the container, skipping cleanup: {}", err);
                return Ok(());
            }
        }
        let container_root = get_instance_root(&self.rootdir, self.id.as_str())?;
        let container = Container::load(container_root).with_context(|| {
            format!(
                "could not load state for container {id}",
                id = self.id.as_str()
            )
        });
        match container {
            Ok(mut container) => container.delete(true).map_err(|err| {
                Error::Any(anyhow::anyhow!(
                    "failed to delete container {}: {}",
                    self.id,
                    err
                ))
            })?,
            Err(err) => {
                error!("could not find the container, skipping cleanup: {}", err);
                return Ok(());
            }
        }

        Ok(())
    }

    fn wait(&self, waiter: &Wait) -> Result<(), Error> {
        log::info!("waiting for instance: {}", self.id);
        let code = self.exit_code.clone();
        waiter.set_up_exit_code_wait(code)
    }
}

impl Wasi {
    fn build_container(&self) -> Result<Container, Error> {
        log::info!("Building container");

        let stdin = maybe_open_stdio(self.stdin.as_str())?;
        let stdout = maybe_open_stdio(self.stdout.as_str())?;
        let stderr = maybe_open_stdio(self.stderr.as_str())?;

        let syscall = create_syscall();
        let err_msg = |err| format!("failed to create container: {}", err);
        let container = ContainerBuilder::new(self.id.clone(), syscall.as_ref())
            .with_executor(vec![Box::new(LunaticExecutor {
                stdin,
                stdout,
                stderr,
            })])
            .map_err(|err| Error::Others(err_msg(err)))?
            .with_root_path(self.rootdir.clone())
            .map_err(|err| Error::Others(err_msg(err)))?
            .as_init(&self.bundle)
            .with_systemd(false)
            .build()
            .map_err(|err| Error::Others(err_msg(err)))?;

        log::info!(">>> Container built.");
        Ok(container)
    }
}

impl EngineGetter for Wasi {
    type E = ();

    fn new_engine() -> std::result::Result<Self::E, Error> {
        Ok(())
    }
}
fn main() {
    run::<ShimCli<Wasi, ()>>("io.containerd.lunatic.v1", None);
}
