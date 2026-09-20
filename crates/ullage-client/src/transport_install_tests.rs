use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixListener;
use std::sync::Mutex;
use std::sync::atomic::AtomicBool;

use ullage_protocol::CredentialBackendId;

use super::*;

// The fake service manager lets tests observe the install/stop/start
// ordering without a real systemd, launchd, or schtasks. One lock serializes
// the two tests because fn pointers share statics.
static SERVICE_FAKE_LOCK: Mutex<()> = Mutex::new(());
static SERVICE_FAKE_INSTALLED: AtomicBool = AtomicBool::new(false);
static SERVICE_FAKE_STOP_ISSUED: AtomicBool = AtomicBool::new(false);
static SERVICE_FAKE_CALLS: Mutex<Vec<&'static str>> = Mutex::new(Vec::new());
static SERVICE_FAKE_START_SOCKET: Mutex<Option<PathBuf>> = Mutex::new(None);

fn fake_service_installed() -> Result<bool, String> {
    SERVICE_FAKE_CALLS.lock().unwrap().push("installed");
    Ok(SERVICE_FAKE_INSTALLED.load(Ordering::Relaxed))
}

fn fake_service_stop() -> Result<bool, String> {
    SERVICE_FAKE_CALLS.lock().unwrap().push("stop");
    Ok(SERVICE_FAKE_STOP_ISSUED.load(Ordering::Relaxed))
}

fn fake_service_manage(action: ServiceAction) -> Result<(), String> {
    match action {
        ServiceAction::Install => {
            SERVICE_FAKE_CALLS.lock().unwrap().push("install");
            SERVICE_FAKE_INSTALLED.store(true, Ordering::Relaxed);
        }
        ServiceAction::Start => {
            SERVICE_FAKE_CALLS.lock().unwrap().push("start");
            // The service manager binds the control socket only now, so the
            // client's endpoint probe must see it become ready afterwards.
            let socket_path = SERVICE_FAKE_START_SOCKET
                .lock()
                .unwrap()
                .clone()
                .ok_or("fake service start has no socket")?;
            let listener = UnixListener::bind(&socket_path)
                .map_err(|_| "fake service socket could not bind".to_owned())?;
            std::fs::set_permissions(&socket_path, std::fs::Permissions::from_mode(0o600))
                .map_err(|_| "fake service socket could not be protected".to_owned())?;
            std::thread::spawn(move || {
                while let Ok((mut stream, _)) = listener.accept() {
                    let mut encoded = String::new();
                    if BufReader::new(&mut stream).read_line(&mut encoded).is_err() {
                        continue;
                    }
                    let Ok(request) = serde_json::from_str::<ControlRequest>(&encoded) else {
                        continue;
                    };
                    let response = ControlResponse {
                        version: CONTROL_PROTOCOL_VERSION,
                        request_id: request.request_id,
                        result: ControlResult::DaemonStatus(DaemonStatusPayload {
                            shutting_down: false,
                            accounts: Vec::new(),
                            credential_backend: CredentialBackendId::native(),
                        }),
                        diagnostic: None,
                        daemon_version: None,
                    };
                    if serde_json::to_writer(&mut stream, &response).is_err()
                        || stream.write_all(b"\n").is_err()
                    {
                        continue;
                    }
                }
            });
        }
        ServiceAction::Stop | ServiceAction::Uninstall => {
            SERVICE_FAKE_CALLS.lock().unwrap().push("manage-other");
        }
    }
    Ok(())
}

const FAKE_SERVICE_OPS: ServiceOps = ServiceOps {
    installed: fake_service_installed,
    stop: fake_service_stop,
    manage: fake_service_manage,
};

fn fake_service_setup(installed: bool, stop_issued: bool) -> (PathBuf, PathBuf) {
    SERVICE_FAKE_INSTALLED.store(installed, Ordering::Relaxed);
    SERVICE_FAKE_STOP_ISSUED.store(stop_issued, Ordering::Relaxed);
    SERVICE_FAKE_CALLS.lock().unwrap().clear();
    let directory = std::env::temp_dir().join(format!(
        "ullage-cli-install-fake-{}-{}",
        std::process::id(),
        REQUEST_SEQUENCE.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir(&directory).unwrap();
    let socket_path = directory.join("control.sock");
    *SERVICE_FAKE_START_SOCKET.lock().unwrap() = Some(socket_path.clone());
    (directory, socket_path)
}

fn fake_service_teardown(directory: PathBuf) {
    *SERVICE_FAKE_START_SOCKET.lock().unwrap() = None;
    let _ = std::fs::remove_file(directory.join("control.sock"));
    std::fs::remove_dir(directory).unwrap();
}

#[test]
fn daemon_install_installs_and_starts_when_nothing_was_installed() {
    let _guard = SERVICE_FAKE_LOCK.lock().unwrap();
    let (directory, socket_path) = fake_service_setup(false, false);
    let client = SystemClient {
        endpoint: Some(socket_path),
    };

    client
        .manage_service_with(ServiceAction::Install, FAKE_SERVICE_OPS)
        .unwrap();

    assert_eq!(
        SERVICE_FAKE_CALLS.lock().unwrap().as_slice(),
        ["stop", "install", "installed", "start"]
    );
    fake_service_teardown(directory);
}

#[test]
fn daemon_install_stops_reinstalls_and_starts_an_installed_service() {
    let _guard = SERVICE_FAKE_LOCK.lock().unwrap();
    let (directory, socket_path) = fake_service_setup(true, true);
    let client = SystemClient {
        endpoint: Some(socket_path),
    };

    client
        .manage_service_with(ServiceAction::Install, FAKE_SERVICE_OPS)
        .unwrap();

    assert_eq!(
        SERVICE_FAKE_CALLS.lock().unwrap().as_slice(),
        ["stop", "install", "installed", "start"]
    );
    fake_service_teardown(directory);
}
