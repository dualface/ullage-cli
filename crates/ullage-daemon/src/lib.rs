//! Scheduling, persistence, and local control for Ullage providers.

use std::sync::Once;

mod control;
mod device;
mod engine;
mod model;
mod store;

#[cfg(unix)]
pub use control::UnixControlServer;
#[cfg(windows)]
pub use control::WindowsControlServer;
pub use control::{ControlService, ControlTransport};
pub use device::{
    DeviceCredential, DeviceStore, PairDeviceError, constant_time_eq, generate_device_token,
};
pub use engine::{Clock, DaemonEngine, SystemClock};
pub use model::{
    AccountConfig, AccountId, AccountStatus, BackoffConfig, DaemonConfig, DaemonError,
    DaemonStatus, FailureRecord, PersistedState, ProbeError, ProbeTrigger, ProviderLimit,
    SanitizedError, SnapshotRecord,
};
pub use store::{JsonSnapshotStore, MemorySnapshotStore, SnapshotStore, StagedSnapshot};

static REDACTING_PANIC_HOOK: Once = Once::new();

/// Installs the daemon process panic hook without exposing panic payloads.
///
/// The hook is process-global and should be installed once by the daemon entry
/// point before provider or storage tasks are started.
pub fn install_redacting_panic_hook() {
    REDACTING_PANIC_HOOK.call_once(|| {
        std::panic::set_hook(Box::new(|_| {
            eprintln!("ullage daemon task panicked");
        }));
    });
}
