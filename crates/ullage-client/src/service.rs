use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use super::ServiceAction;

#[cfg(any(target_os = "macos", test))]
const LABEL: &str = "dev.onevoke.ullage";

pub(super) fn manage(action: ServiceAction) -> Result<(), String> {
    manage_action(action).map(|_| ())
}

pub(super) fn stop() -> Result<bool, String> {
    manage_action(ServiceAction::Stop)
}

fn manage_action(action: ServiceAction) -> Result<bool, String> {
    let executable = std::env::current_exe().map_err(|_| "service executable unavailable")?;
    if !executable.is_absolute() {
        return Err("service executable must be absolute".into());
    }
    validate_executable(&executable)?;
    #[cfg(target_os = "linux")]
    return linux::manage(action, &executable);
    #[cfg(target_os = "macos")]
    return macos::manage(action, &executable);
    #[cfg(windows)]
    return windows::manage(action, &executable);
    #[allow(unreachable_code)]
    Err("service hosting is unsupported on this platform".into())
}

pub(super) fn installed() -> Result<bool, String> {
    #[cfg(target_os = "linux")]
    return private_manifest_exists(&linux::unit_path()?);
    #[cfg(target_os = "macos")]
    return private_manifest_exists(&macos::plist_path()?);
    #[cfg(windows)]
    return windows::installed();
    #[allow(unreachable_code)]
    Err("service hosting is unsupported on this platform".into())
}

#[cfg(unix)]
fn private_manifest_exists(path: &Path) -> Result<bool, String> {
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt};

    match std::fs::symlink_metadata(path) {
        Ok(_) => {
            validate_unix_ancestors(path.parent().ok_or("service manifest has no parent")?)?;
            let file = std::fs::OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
                .open(path)
                .map_err(|_| "service manifest could not be opened safely")?;
            let metadata = file
                .metadata()
                .map_err(|_| "service manifest could not be validated")?;
            if !metadata.is_file() || metadata.uid() != unsafe { libc::geteuid() } {
                return Err("service manifest must be a current-user-owned regular file".into());
            }
            Ok(true)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(_) => Err("service entry could not be inspected".into()),
    }
}

#[cfg(unix)]
fn validate_executable(executable: &Path) -> Result<(), String> {
    use std::os::unix::fs::MetadataExt;

    let metadata = std::fs::symlink_metadata(executable)
        .map_err(|_| "service executable could not be validated")?;
    let current_user = unsafe { libc::geteuid() };
    if !metadata.is_file()
        || metadata.file_type().is_symlink()
        || (metadata.uid() != 0 && metadata.uid() != current_user)
    {
        return Err(
            "service executable must be a regular file owned by the current user or root".into(),
        );
    }
    let mut ancestor = executable.parent();
    while let Some(path) = ancestor {
        let metadata = std::fs::symlink_metadata(path)
            .map_err(|_| "service executable path could not be validated")?;
        if metadata.file_type().is_symlink()
            || (metadata.uid() != 0 && metadata.uid() != current_user)
        {
            return Err("service executable path contains an unsafe ancestor".into());
        }
        ancestor = path.parent();
    }
    Ok(())
}

#[cfg(windows)]
fn validate_executable(executable: &Path) -> Result<(), String> {
    use std::os::windows::fs::MetadataExt;
    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
    let metadata = std::fs::symlink_metadata(executable)
        .map_err(|_| "service executable could not be validated")?;
    if !metadata.is_file() || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        return Err("service executable must be a regular non-reparse file".into());
    }
    Ok(())
}

fn run(program: &Path, arguments: &[OsString]) -> Result<bool, String> {
    let status = Command::new(program)
        .args(arguments)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map_err(|_| "service manager unavailable")?;
    Ok(status.success())
}

fn require(program: &Path, arguments: &[OsString]) -> Result<(), String> {
    if run(program, arguments)? {
        Ok(())
    } else {
        Err("service manager rejected the request".into())
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn write_private(path: &Path, contents: &[u8]) -> Result<(), String> {
    let parent = path.parent().ok_or("service path has no parent")?;
    validate_unix_ancestors(parent)?;
    create_private_directories(parent)?;
    validate_unix_ancestors(parent)?;
    if std::fs::symlink_metadata(path).is_ok_and(|metadata| metadata.file_type().is_symlink()) {
        return Err("service file must not be a symbolic link".into());
    }
    let temporary = path.with_extension(format!("tmp-{}", std::process::id()));
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(&temporary)
        .map_err(|_| "private service file could not be created")?;
    let result = (|| {
        use std::io::Write;
        file.write_all(contents)
            .map_err(|_| "service file could not be written".to_owned())?;
        file.sync_all()
            .map_err(|_| "service file could not be synchronized".to_owned())?;
        std::fs::rename(&temporary, path)
            .map_err(|_| "service file could not be installed".to_owned())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    result
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn create_private_directories(path: &Path) -> Result<(), String> {
    use std::os::unix::fs::DirBuilderExt;
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true).mode(0o700);
    builder
        .create(path)
        .map_err(|_| "service directory could not be created".into())
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn validate_unix_ancestors(path: &Path) -> Result<(), String> {
    use std::os::unix::fs::MetadataExt;

    let current_user = unsafe { libc::geteuid() };
    let mut current = PathBuf::new();
    for component in path.components() {
        current.push(component.as_os_str());
        match std::fs::symlink_metadata(&current) {
            Ok(metadata)
                if metadata.file_type().is_symlink()
                    || (metadata.uid() != 0 && metadata.uid() != current_user) =>
            {
                return Err("service path contains an unsafe ancestor".into());
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => return Err("service path could not be validated".into()),
        }
    }
    Ok(())
}

#[cfg(target_os = "linux")]
mod linux {
    use super::*;

    pub(super) fn manage(action: ServiceAction, executable: &Path) -> Result<bool, String> {
        let unit = unit_path()?;
        let systemctl = [Path::new("/usr/bin/systemctl"), Path::new("/bin/systemctl")]
            .into_iter()
            .find(|path| path.is_file())
            .ok_or("systemctl unavailable")?;
        match action {
            ServiceAction::Install => {
                write_private(&unit, render_unit(executable)?.as_bytes())?;
                require(systemctl, &args(&["--user", "daemon-reload"]))?;
                require(systemctl, &args(&["--user", "enable", "ullage.service"]))?;
                Ok(false)
            }
            ServiceAction::Start => {
                if !private_manifest_exists(&unit)? {
                    return Err("daemon service is not installed".into());
                }
                require(systemctl, &args(&["--user", "start", "ullage.service"]))?;
                Ok(false)
            }
            ServiceAction::Stop => {
                let installed = private_manifest_exists(&unit)?;
                let active = installed
                    && run(
                        systemctl,
                        &args(&["--user", "is-active", "--quiet", "ullage.service"]),
                    )?;
                if installed {
                    require(systemctl, &args(&["--user", "stop", "ullage.service"]))?;
                }
                Ok(active)
            }
            ServiceAction::Uninstall => {
                if private_manifest_exists(&unit)? {
                    require(systemctl, &args(&["--user", "disable", "ullage.service"]))?;
                    std::fs::remove_file(&unit).map_err(|_| "service file could not be removed")?;
                }
                require(systemctl, &args(&["--user", "daemon-reload"]))?;
                Ok(false)
            }
        }
    }

    fn args(values: &[&str]) -> Vec<OsString> {
        values.iter().map(OsString::from).collect()
    }

    pub(super) fn unit_path() -> Result<PathBuf, String> {
        if let Some(root) = std::env::var_os("XDG_CONFIG_HOME") {
            return Ok(PathBuf::from(root).join("systemd/user/ullage.service"));
        }
        std::env::var_os("HOME")
            .map(PathBuf::from)
            .map(|home| home.join(".config/systemd/user/ullage.service"))
            .ok_or_else(|| "home directory unavailable".into())
    }
}

#[cfg(target_os = "macos")]
mod macos {
    use super::*;

    pub(super) fn manage(action: ServiceAction, executable: &Path) -> Result<bool, String> {
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .ok_or("home directory unavailable")?;
        let plist = plist_path()?;
        let target = format!("gui/{}/{LABEL}", unsafe { libc::geteuid() });
        let launchctl = Path::new("/bin/launchctl");
        match action {
            ServiceAction::Install => {
                let log_directory = home.join("Library/Logs/Ullage");
                validate_unix_ancestors(&log_directory)?;
                let created_log_directory = !log_directory.exists();
                create_private_directories(&log_directory)
                    .map_err(|_| "service log directory could not be created")?;
                if created_log_directory {
                    use std::os::unix::fs::PermissionsExt;
                    std::fs::set_permissions(
                        &log_directory,
                        std::fs::Permissions::from_mode(0o700),
                    )
                    .map_err(|_| "service log directory could not be protected")?;
                }
                validate_unix_ancestors(&log_directory)?;
                validate_private_directory(&log_directory)?;
                ensure_private_log_file(&log_directory.join("ullage.log"))?;
                ensure_private_log_file(&log_directory.join("ullage-error.log"))?;
                write_private(&plist, render_plist(executable, &home)?.as_bytes())?;
                Ok(false)
            }
            ServiceAction::Start => {
                if !private_manifest_exists(&plist)? {
                    return Err("daemon service is not installed".into());
                }
                match loaded(launchctl, &target)? {
                    LoadState::Loaded => require(
                        launchctl,
                        &[OsString::from("kickstart"), OsString::from(&target)],
                    ),
                    LoadState::Absent => require(
                        launchctl,
                        &[
                            OsString::from("bootstrap"),
                            OsString::from(format!("gui/{}", unsafe { libc::geteuid() })),
                            plist.into_os_string(),
                        ],
                    ),
                }?;
                Ok(false)
            }
            ServiceAction::Stop => {
                let _installed = private_manifest_exists(&plist)?;
                match loaded(launchctl, &target)? {
                    LoadState::Loaded => {
                        require(
                            launchctl,
                            &[OsString::from("bootout"), OsString::from(&target)],
                        )?;
                        Ok(true)
                    }
                    LoadState::Absent => Ok(false),
                }
            }
            ServiceAction::Uninstall => {
                if private_manifest_exists(&plist)? {
                    std::fs::remove_file(plist).map_err(|_| "service file could not be removed")?;
                }
                Ok(false)
            }
        }
    }

    enum LoadState {
        Loaded,
        Absent,
    }

    fn loaded(launchctl: &Path, target: &str) -> Result<LoadState, String> {
        let output = Command::new(launchctl)
            .args([std::ffi::OsStr::new("print"), std::ffi::OsStr::new(target)])
            .stdin(Stdio::null())
            .output()
            .map_err(|_| "launchctl unavailable")?;
        if output.status.success() {
            return Ok(LoadState::Loaded);
        }
        let error = String::from_utf8_lossy(&output.stderr);
        if error.contains("Could not find service") {
            Ok(LoadState::Absent)
        } else {
            Err("launchctl service query failed".into())
        }
    }

    pub(super) fn plist_path() -> Result<PathBuf, String> {
        std::env::var_os("HOME")
            .map(PathBuf::from)
            .map(|home| home.join(format!("Library/LaunchAgents/{LABEL}.plist")))
            .ok_or_else(|| "home directory unavailable".into())
    }

    fn validate_private_directory(path: &Path) -> Result<(), String> {
        use std::os::unix::fs::MetadataExt;
        let metadata = std::fs::symlink_metadata(path)
            .map_err(|_| "service log directory could not be validated")?;
        if !metadata.is_dir()
            || metadata.file_type().is_symlink()
            || metadata.uid() != unsafe { libc::geteuid() }
        {
            return Err("service log directory must be a current-user-owned directory".into());
        }
        Ok(())
    }

    fn ensure_private_log_file(path: &Path) -> Result<(), String> {
        use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
        let file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
            .mode(0o600)
            .open(path)
            .map_err(|_| "service log file could not be opened safely")?;
        let metadata = file
            .metadata()
            .map_err(|_| "service log file could not be validated")?;
        if !metadata.is_file() || metadata.uid() != unsafe { libc::geteuid() } {
            return Err("service log file must be a current-user-owned regular file".into());
        }
        Ok(())
    }
}

#[cfg(windows)]
mod windows {
    use super::*;

    pub(super) fn manage(action: ServiceAction, executable: &Path) -> Result<bool, String> {
        let _validated_path = validate_windows_path(executable)?;
        let schtasks = system_tool("schtasks.exe")?;
        match action {
            ServiceAction::Install => match task_marker_state()? {
                Some(TaskMarkerState::Installed(task)) => {
                    require_task_definition(&schtasks, &task)?;
                    require(&schtasks, &args(&["/Query", "/TN", task.as_str()]))?;
                    let arguments = windows_create_arguments(executable, &task, true)?;
                    require(&schtasks, &arguments)
                }
                Some(TaskMarkerState::Pending(task)) => {
                    recover_pending_install(&schtasks, executable, &task)
                }
                None => {
                    let task = new_task_name()?;
                    let arguments = windows_create_arguments(executable, &task, false)?;
                    create_pending_task_marker(&task)?;
                    if let Err(create_error) = require(&schtasks, &arguments) {
                        return match remove_task_marker() {
                            Ok(()) => Err(create_error),
                            Err(_) => Err("daemon task creation and marker rollback failed".into()),
                        };
                    }
                    let mut marker = open_task_marker_for_update()?;
                    finalize_task_marker(&mut marker)
                }
            }
            .map(|_| false),
            ServiceAction::Start => {
                let task = match task_marker_state()? {
                    Some(TaskMarkerState::Installed(task)) => task,
                    Some(TaskMarkerState::Pending(_)) => {
                        return Err("daemon task installation is incomplete".into());
                    }
                    None => return Err("daemon task is not installed".into()),
                };
                require_task_definition(&schtasks, &task)?;
                require(&schtasks, &args(&["/Query", "/TN", task.as_str()]))?;
                require(&schtasks, &args(&["/Run", "/TN", task.as_str()]))?;
                Ok(false)
            }
            ServiceAction::Stop => {
                let stopped = match task_marker_state()? {
                    Some(TaskMarkerState::Installed(task)) => {
                        require_task_definition(&schtasks, &task)?;
                        stop_registered_task(&schtasks, &task)?
                    }
                    Some(TaskMarkerState::Pending(task)) => stop_pending_task(&schtasks, &task)?,
                    None => false,
                };
                Ok(stopped)
            }
            ServiceAction::Uninstall => {
                match task_marker_state()? {
                    Some(TaskMarkerState::Installed(task)) => {
                        require_task_definition(&schtasks, &task)?;
                        require(&schtasks, &args(&["/Delete", "/TN", task.as_str(), "/F"]))?;
                        remove_task_marker()?;
                    }
                    Some(TaskMarkerState::Pending(task)) => {
                        match pending_uninstall_recovery(task_definition_exists(&schtasks, &task)?)
                        {
                            PendingUninstallRecovery::DeleteTask => {
                                require(&schtasks, &args(&["/Delete", "/TN", task.as_str(), "/F"]))?
                            }
                            PendingUninstallRecovery::RemoveMarker => {}
                        }
                        remove_task_marker()?;
                    }
                    None => {}
                }
                Ok(false)
            }
        }
    }

    pub(super) fn installed() -> Result<bool, String> {
        let schtasks = system_tool("schtasks.exe")?;
        let task = match task_marker_state()? {
            None => return Ok(false),
            Some(TaskMarkerState::Pending(_)) => {
                return Err("daemon task installation is incomplete".into());
            }
            Some(TaskMarkerState::Installed(task)) => task,
        };
        require_task_definition(&schtasks, &task)?;
        require(&schtasks, &args(&["/Query", "/TN", task.as_str()]))?;
        Ok(true)
    }

    fn new_task_name() -> Result<String, String> {
        let scope = ullage_auth::current_windows_user_scope()
            .map_err(|_| "current Windows user identity unavailable")?;
        let nonce = ullage_auth::new_windows_service_nonce()
            .map_err(|_| "Windows service identity could not be generated")?;
        Ok(format!("Ullage-{scope}-{nonce}"))
    }

    fn task_marker_path() -> Result<PathBuf, String> {
        std::env::var_os("LOCALAPPDATA")
            .map(PathBuf::from)
            .map(|root| root.join("Ullage/service-task"))
            .ok_or_else(|| "Windows local data directory unavailable".into())
    }

    enum TaskMarkerState {
        Pending(String),
        Installed(String),
    }

    fn create_pending_task_marker(task: &str) -> Result<(), String> {
        use std::io::Write;
        let path = task_marker_path()?;
        let parent = path.parent().ok_or("service marker path has no parent")?;
        if !parent.exists() {
            ullage_auth::create_private_windows_directory(parent)
                .map_err(|_| "private service marker directory could not be created")?;
        }
        let _validated_parent = validate_windows_path(parent)?;
        if task_marker_state()?.is_some() {
            return Err("service marker already exists".into());
        }
        let nonce = task
            .rsplit('-')
            .next()
            .ok_or("service task name is invalid")?;
        let temporary = path.with_extension(format!("pending-{nonce}"));
        let mut file = ullage_auth::create_private_windows_file(&temporary)
            .map_err(|_| "private service marker could not be created")?;
        let result = file
            .write_all(format!("0\n{task}\n").as_bytes())
            .map_err(|_| "service marker could not be written".to_owned())
            .and_then(|()| {
                file.sync_all()
                    .map_err(|_| "service marker could not be synchronized".to_owned())
            });
        if let Err(error) = result {
            drop(file);
            let _ = std::fs::remove_file(temporary);
            return Err(error);
        }
        drop(file);
        match std::fs::rename(&temporary, &path) {
            Ok(()) => Ok(()),
            Err(_) => {
                let _ = std::fs::remove_file(temporary);
                Err("service marker could not be installed".into())
            }
        }
    }

    fn finalize_task_marker(file: &mut std::fs::File) -> Result<(), String> {
        use std::io::{Seek, Write};
        file.seek(std::io::SeekFrom::Start(0))
            .map_err(|_| "service marker could not be finalized")?;
        file.write_all(b"1")
            .map_err(|_| "service marker could not be finalized")?;
        file.sync_all()
            .map_err(|_| "service marker could not be synchronized".to_owned())
    }

    fn task_marker_state() -> Result<Option<TaskMarkerState>, String> {
        use std::io::Read;
        use std::os::windows::io::AsRawHandle;
        let path = task_marker_path()?;
        match std::fs::symlink_metadata(&path) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(_) => Err("service marker could not be inspected".into()),
            Ok(metadata) if !metadata.is_file() => {
                Err("service marker must be a regular file".into())
            }
            Ok(_) => {
                let handles = validate_windows_path(&path)?;
                let marker = handles.first().ok_or("service marker handle unavailable")?;
                if ullage_auth::windows_handle_acl_is_private(marker.as_raw_handle())
                    .map_err(|_| "service marker ACL could not be inspected")?
                {
                    let mut marker = std::fs::OpenOptions::new()
                        .read(true)
                        .open(&path)
                        .map_err(|_| "service marker could not be read")?;
                    let mut contents = String::new();
                    marker
                        .by_ref()
                        .take(193)
                        .read_to_string(&mut contents)
                        .map_err(|_| "service marker could not be read")?;
                    if contents.len() > 192 {
                        return Err("service marker contents are invalid".into());
                    }
                    parse_task_marker(&contents).map(Some)
                } else {
                    Err("service marker must be private".into())
                }
            }
        }
    }

    fn parse_task_marker(contents: &str) -> Result<TaskMarkerState, String> {
        let mut lines = contents.split_terminator('\n');
        let state = lines.next().ok_or("service marker contents are invalid")?;
        let task = lines.next().ok_or("service marker contents are invalid")?;
        if lines.next().is_some() || !valid_task_name(task)? {
            return Err("service marker contents are invalid".into());
        }
        match state {
            "0" => Ok(TaskMarkerState::Pending(task.to_owned())),
            "1" => Ok(TaskMarkerState::Installed(task.to_owned())),
            _ => Err("service marker contents are invalid".into()),
        }
    }

    fn valid_task_name(task: &str) -> Result<bool, String> {
        let scope = ullage_auth::current_windows_user_scope()
            .map_err(|_| "current Windows user identity unavailable")?;
        let prefix = format!("Ullage-{scope}-");
        let nonce = task.strip_prefix(&prefix);
        Ok(nonce.is_some_and(|value| {
            value.len() == 32
                && value
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        }))
    }

    fn recover_pending_install(
        schtasks: &Path,
        executable: &Path,
        task: &str,
    ) -> Result<(), String> {
        match pending_install_recovery(task_definition_exists(schtasks, task)?) {
            PendingInstallRecovery::CreateTask => {
                let arguments = windows_create_arguments(executable, task, false)?;
                require(schtasks, &arguments)?;
            }
            PendingInstallRecovery::FinalizeMarker => {
                require(schtasks, &args(&["/Query", "/TN", task]))?;
            }
        }
        let mut marker = open_task_marker_for_update()?;
        finalize_task_marker(&mut marker)
    }

    fn stop_pending_task(schtasks: &Path, task: &str) -> Result<bool, String> {
        if task_definition_exists(schtasks, task)? {
            return stop_registered_task(schtasks, task);
        }
        Ok(false)
    }

    fn stop_registered_task(schtasks: &Path, task: &str) -> Result<bool, String> {
        let running = task_is_running(task)?;
        if running {
            require(schtasks, &args(&["/End", "/TN", task]))?;
        }
        Ok(running)
    }

    fn task_is_running(task: &str) -> Result<bool, String> {
        use std::os::windows::ffi::OsStrExt;
        use std::ptr::{NonNull, null_mut};
        use winapi::shared::winerror::SUCCEEDED;
        use winapi::shared::wtypesbase::CLSCTX_INPROC_SERVER;
        use winapi::um::combaseapi::{CoCreateInstance, CoInitializeEx, CoUninitialize};
        use winapi::um::oaidl::VARIANT;
        use winapi::um::objbase::COINIT_MULTITHREADED;
        use winapi::um::oleauto::{SysAllocString, SysFreeString};
        use winapi::um::taskschd::{
            IRegisteredTask, ITaskFolder, ITaskService, TASK_STATE_RUNNING, TaskScheduler,
        };
        use winapi::{Class, Interface};

        struct ComApartment;
        impl Drop for ComApartment {
            fn drop(&mut self) {
                unsafe { CoUninitialize() };
            }
        }

        struct ComPointer<T>(*mut T);
        impl<T> Drop for ComPointer<T> {
            fn drop(&mut self) {
                unsafe {
                    (*(self.0 as *mut winapi::um::unknwnbase::IUnknown)).Release();
                }
            }
        }

        struct BString(winapi::shared::wtypes::BSTR);
        impl Drop for BString {
            fn drop(&mut self) {
                unsafe { SysFreeString(self.0) };
            }
        }

        fn bstring(value: &std::ffi::OsStr) -> Result<BString, String> {
            let mut wide: Vec<u16> = value.encode_wide().collect();
            wide.push(0);
            NonNull::new(unsafe { SysAllocString(wide.as_ptr()) })
                .map(|value| BString(value.as_ptr()))
                .ok_or_else(|| "Task Scheduler query allocation failed".into())
        }

        let initialized = unsafe { CoInitializeEx(null_mut(), COINIT_MULTITHREADED) };
        if !SUCCEEDED(initialized) {
            return Err("Task Scheduler COM initialization failed".into());
        }
        let _apartment = ComApartment;
        let mut service = null_mut();
        let created = unsafe {
            CoCreateInstance(
                &TaskScheduler::uuidof(),
                null_mut(),
                CLSCTX_INPROC_SERVER,
                &ITaskService::uuidof(),
                &mut service,
            )
        };
        if !SUCCEEDED(created) || service.is_null() {
            return Err("Task Scheduler query failed".into());
        }
        let service = ComPointer(service.cast::<ITaskService>());
        let empty: VARIANT = unsafe { std::mem::zeroed() };
        if !SUCCEEDED(unsafe { (*service.0).Connect(empty, empty, empty, empty) }) {
            return Err("Task Scheduler connection failed".into());
        }
        let root_path = bstring(std::ffi::OsStr::new("\\"))?;
        let mut folder = null_mut();
        if !SUCCEEDED(unsafe { (*service.0).GetFolder(root_path.0, &mut folder) })
            || folder.is_null()
        {
            return Err("Task Scheduler root could not be opened".into());
        }
        let folder = ComPointer(folder.cast::<ITaskFolder>());
        let task_name = bstring(std::ffi::OsStr::new(task))?;
        let mut registered = null_mut();
        if !SUCCEEDED(unsafe { (*folder.0).GetTask(task_name.0, &mut registered) })
            || registered.is_null()
        {
            return Err("Task Scheduler definition could not be opened".into());
        }
        let registered = ComPointer(registered.cast::<IRegisteredTask>());
        let mut state = 0;
        if !SUCCEEDED(unsafe { (*registered.0).get_State(&mut state) }) {
            return Err("Task Scheduler state query failed".into());
        }
        Ok(state == TASK_STATE_RUNNING)
    }

    fn open_task_marker_for_update() -> Result<std::fs::File, String> {
        use std::os::windows::io::AsRawHandle;
        let path = task_marker_path()?;
        let handles = validate_windows_path(&path)?;
        let marker = handles.first().ok_or("service marker handle unavailable")?;
        if !ullage_auth::windows_handle_acl_is_private(marker.as_raw_handle())
            .map_err(|_| "service marker ACL could not be inspected")?
        {
            return Err("service marker must be private".into());
        }
        drop(handles);
        std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
            .map_err(|_| "service marker could not be reopened safely".into())
    }

    fn require_task_definition(schtasks: &Path, task: &str) -> Result<(), String> {
        if task_definition_exists(schtasks, task)? {
            Ok(())
        } else {
            Err("the Ullage task marker and Task Scheduler definition disagree".into())
        }
    }

    fn remove_task_marker() -> Result<(), String> {
        let path = task_marker_path()?;
        if task_marker_state()?.is_some() {
            std::fs::remove_file(path).map_err(|_| "service marker could not be removed")?;
        }
        Ok(())
    }

    fn args(values: &[&str]) -> Vec<OsString> {
        values.iter().map(OsString::from).collect()
    }
    fn system_tool(name: &str) -> Result<PathBuf, String> {
        let path = system_directory()?.join(name);
        if path.is_file() {
            Ok(path)
        } else {
            Err("Windows service manager unavailable".into())
        }
    }

    fn system_directory() -> Result<PathBuf, String> {
        use std::os::windows::ffi::OsStringExt;
        use windows_sys::Win32::System::SystemInformation::GetSystemDirectoryW;

        let mut buffer = vec![0u16; 32_768];
        let length = unsafe { GetSystemDirectoryW(buffer.as_mut_ptr(), buffer.len() as u32) };
        if length == 0 || length as usize >= buffer.len() {
            return Err("Windows system directory unavailable".into());
        }
        buffer.truncate(length as usize);
        Ok(PathBuf::from(OsString::from_wide(&buffer)))
    }

    fn task_definition_exists(schtasks: &Path, task: &str) -> Result<bool, String> {
        run(schtasks, &args(&["/Query", "/TN", task]))
    }

    fn validate_windows_path(executable: &Path) -> Result<Vec<std::fs::File>, String> {
        use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
        use windows_sys::Win32::Storage::FileSystem::{
            FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
            FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, READ_CONTROL,
        };

        let mut handles = Vec::new();
        let mut current = Some(executable);
        let mut depth = 0;
        while let Some(path) = current {
            let metadata = std::fs::symlink_metadata(path)
                .map_err(|_| "service executable path could not be validated")?;
            if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
                return Err("service executable path must not contain reparse points".into());
            }
            let mut options = std::fs::OpenOptions::new();
            options
                .access_mode(READ_CONTROL)
                .share_mode(if depth < 2 {
                    FILE_SHARE_READ
                } else {
                    FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE
                })
                .custom_flags(
                    FILE_FLAG_OPEN_REPARSE_POINT
                        | if metadata.is_dir() {
                            FILE_FLAG_BACKUP_SEMANTICS
                        } else {
                            0
                        },
                );
            let file = options
                .open(path)
                .map_err(|_| "service executable path could not be fixed safely")?;
            let opened = file
                .metadata()
                .map_err(|_| "service executable path could not be validated")?;
            if opened.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
                return Err("service executable path must not contain reparse points".into());
            }
            handles.push(file);
            current = path.parent();
            depth += 1;
        }
        Ok(handles)
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn validate_windows_path_accepts_a_user_owned_temp_file() {
            let root =
                std::env::temp_dir().join(format!("ullage-windows-path-{}", std::process::id()));
            std::fs::create_dir_all(&root).unwrap();
            let executable = root.join("ullage.exe");
            std::fs::write(&executable, b"test").unwrap();
            let handles = validate_windows_path(&executable).unwrap();
            drop(handles);
            std::fs::remove_file(&executable).unwrap();
            std::fs::remove_dir(&root).unwrap();
        }
    }
}

#[cfg(any(windows, test))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PendingInstallRecovery {
    CreateTask,
    FinalizeMarker,
}

#[cfg(any(windows, test))]
const fn pending_install_recovery(task_exists: bool) -> PendingInstallRecovery {
    if task_exists {
        PendingInstallRecovery::FinalizeMarker
    } else {
        PendingInstallRecovery::CreateTask
    }
}

#[cfg(any(windows, test))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PendingUninstallRecovery {
    DeleteTask,
    RemoveMarker,
}

#[cfg(any(windows, test))]
const fn pending_uninstall_recovery(task_exists: bool) -> PendingUninstallRecovery {
    if task_exists {
        PendingUninstallRecovery::DeleteTask
    } else {
        PendingUninstallRecovery::RemoveMarker
    }
}

#[cfg(any(windows, test))]
fn windows_create_arguments(
    executable: &Path,
    task: &str,
    replace: bool,
) -> Result<Vec<OsString>, String> {
    let command = quote_task_command(executable)?;
    let mut arguments: Vec<OsString> = ["/Create", "/SC", "ONLOGON", "/TN", task, "/TR"]
        .into_iter()
        .map(OsString::from)
        .chain([OsString::from(command)])
        .collect();
    if replace {
        arguments.push(OsString::from("/F"));
    }
    arguments.extend(["/RL", "LIMITED"].into_iter().map(OsString::from));
    Ok(arguments)
}

#[cfg(any(target_os = "linux", test))]
fn render_unit(executable: &Path) -> Result<String, String> {
    let executable = executable
        .to_str()
        .ok_or("service executable path is not Unicode")?;
    if executable.chars().any(|character| character.is_control()) {
        return Err("service executable path contains control characters".into());
    }
    let escaped = executable
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('%', "%%")
        .replace('$', "$$");
    Ok(format!(
        "[Unit]\nDescription=Ullage subscription usage daemon\n\n[Service]\nType=simple\nExecStart=\"{escaped}\" __daemon\nRestart=on-failure\nRestartSec=5\nUMask=0077\nNoNewPrivileges=true\n\n[Install]\nWantedBy=default.target\n"
    ))
}

#[cfg(any(target_os = "macos", test))]
fn render_plist(executable: &Path, home: &Path) -> Result<String, String> {
    let executable = xml_escape(
        executable
            .to_str()
            .ok_or("service executable path is not Unicode")?,
    )?;
    let log_dir = home.join("Library/Logs/Ullage");
    let log = xml_escape(
        log_dir
            .join("ullage.log")
            .to_str()
            .ok_or("service log path is not Unicode")?,
    )?;
    let error_log = xml_escape(
        log_dir
            .join("ullage-error.log")
            .to_str()
            .ok_or("service log path is not Unicode")?,
    )?;
    Ok(format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n<plist version=\"1.0\"><dict>\n<key>Label</key><string>{LABEL}</string>\n<key>ProgramArguments</key><array><string>{executable}</string><string>__daemon</string></array>\n<key>RunAtLoad</key><true/>\n<key>KeepAlive</key><dict><key>SuccessfulExit</key><false/></dict>\n<key>StandardOutPath</key><string>{log}</string>\n<key>StandardErrorPath</key><string>{error_log}</string>\n<key>Umask</key><integer>63</integer>\n</dict></plist>\n"
    ))
}

#[cfg(any(target_os = "macos", test))]
fn xml_escape(value: &str) -> Result<String, String> {
    if value.chars().any(|character| character.is_control()) {
        return Err("service path contains control characters".into());
    }
    Ok(value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;"))
}

#[cfg(any(windows, test))]
fn quote_task_command(executable: &Path) -> Result<String, String> {
    let executable = executable
        .to_str()
        .ok_or("service executable path is not Unicode")?;
    if executable.contains(['"', '%']) || executable.chars().any(|character| character.is_control())
    {
        return Err("service executable path cannot be safely quoted".into());
    }
    Ok(format!("\"{executable}\" __daemon"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn systemd_unit_quotes_executable_and_hardens_permissions() {
        let unit = render_unit(Path::new("/opt/Ullage %i/$release/ullage")).unwrap();
        assert_eq!(
            unit,
            "[Unit]\nDescription=Ullage subscription usage daemon\n\n[Service]\nType=simple\nExecStart=\"/opt/Ullage %%i/$$release/ullage\" __daemon\nRestart=on-failure\nRestartSec=5\nUMask=0077\nNoNewPrivileges=true\n\n[Install]\nWantedBy=default.target\n"
        );
    }

    #[test]
    fn launchd_plist_uses_argument_array_and_standard_log_directory() {
        let plist = render_plist(
            Path::new("/Applications/Ullage & Tools/ullage"),
            Path::new("/Users/test"),
        )
        .unwrap();
        assert_eq!(
            plist,
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n<plist version=\"1.0\"><dict>\n<key>Label</key><string>dev.onevoke.ullage</string>\n<key>ProgramArguments</key><array><string>/Applications/Ullage &amp; Tools/ullage</string><string>__daemon</string></array>\n<key>RunAtLoad</key><true/>\n<key>KeepAlive</key><dict><key>SuccessfulExit</key><false/></dict>\n<key>StandardOutPath</key><string>/Users/test/Library/Logs/Ullage/ullage.log</string>\n<key>StandardErrorPath</key><string>/Users/test/Library/Logs/Ullage/ullage-error.log</string>\n<key>Umask</key><integer>63</integer>\n</dict></plist>\n"
        );
    }

    #[test]
    fn templates_reject_control_characters() {
        assert!(render_unit(Path::new("/tmp/ullage\nother")).is_err());
        assert!(render_plist(Path::new("/tmp/ullage"), Path::new("/Users/bad\nuser")).is_err());
        assert!(quote_task_command(Path::new("C:\\Ullage\nother.exe")).is_err());
        assert!(quote_task_command(Path::new(r"C:\%TEMP%\ullage.exe")).is_err());
    }

    #[test]
    fn windows_task_command_quotes_the_executable_without_a_shell() {
        let executable = Path::new(r"C:\Program Files\Ullage\ullage.exe");
        let task = "Ullage-user-scope";
        assert_eq!(
            quote_task_command(executable).unwrap(),
            r#""C:\Program Files\Ullage\ullage.exe" __daemon"#
        );
        assert_eq!(
            windows_create_arguments(executable, task, false).unwrap(),
            [
                "/Create",
                "/SC",
                "ONLOGON",
                "/TN",
                "Ullage-user-scope",
                "/TR",
                r#""C:\Program Files\Ullage\ullage.exe" __daemon"#,
                "/RL",
                "LIMITED",
            ]
            .map(OsString::from)
        );
        assert_eq!(
            windows_create_arguments(executable, task, true).unwrap(),
            [
                "/Create",
                "/SC",
                "ONLOGON",
                "/TN",
                "Ullage-user-scope",
                "/TR",
                r#""C:\Program Files\Ullage\ullage.exe" __daemon"#,
                "/F",
                "/RL",
                "LIMITED",
            ]
            .map(OsString::from)
        );
    }

    #[test]
    fn pending_windows_task_recovers_with_or_without_a_task_definition() {
        assert_eq!(
            pending_install_recovery(false),
            PendingInstallRecovery::CreateTask
        );
        assert_eq!(
            pending_install_recovery(true),
            PendingInstallRecovery::FinalizeMarker
        );
        assert_eq!(
            pending_uninstall_recovery(false),
            PendingUninstallRecovery::RemoveMarker
        );
        assert_eq!(
            pending_uninstall_recovery(true),
            PendingUninstallRecovery::DeleteTask
        );
    }

    #[cfg(unix)]
    #[test]
    fn service_files_accept_group_writable_ancestors() {
        use std::os::unix::fs::PermissionsExt;

        let root = std::env::temp_dir().canonicalize().unwrap().join(format!(
            "ullage-service-group-writable-{}",
            std::process::id()
        ));
        let parent = root.join("group-writable");
        std::fs::create_dir_all(&parent).unwrap();
        std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o775)).unwrap();
        let path = parent.join("ullage.service");
        let result = write_private(&path, b"test");
        let written = std::fs::read(&path).ok();
        let mode = std::fs::metadata(&path)
            .ok()
            .map(|metadata| metadata.permissions().mode() & 0o777);
        let _ = std::fs::remove_file(&path);
        std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o700)).unwrap();
        std::fs::remove_dir(parent).unwrap();
        std::fs::remove_dir(root).unwrap();

        assert_eq!(result, Ok(()));
        assert_eq!(written.as_deref(), Some(b"test".as_slice()));
        assert_eq!(mode, Some(0o600));
    }

    #[cfg(unix)]
    #[test]
    fn service_files_reject_symlink_ancestors() {
        let root = std::env::temp_dir().canonicalize().unwrap().join(format!(
            "ullage-service-symlink-ancestor-{}",
            std::process::id()
        ));
        let real = root.join("real");
        std::fs::create_dir_all(&real).unwrap();
        let link = root.join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let result = write_private(&link.join("ullage.service"), b"test");
        std::fs::remove_file(&link).unwrap();
        std::fs::remove_dir(real).unwrap();
        std::fs::remove_dir(root).unwrap();

        assert!(result.is_err());
    }

    #[cfg(unix)]
    #[test]
    fn group_writable_executable_passes_validation() {
        use std::os::unix::fs::PermissionsExt;

        let root = std::env::temp_dir()
            .canonicalize()
            .unwrap()
            .join(format!("ullage-group-writable-exe-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o775)).unwrap();
        let executable = root.join("ullage");
        std::fs::write(&executable, b"#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o775)).unwrap();

        let result = validate_executable(&executable);

        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();
        std::fs::remove_file(&executable).unwrap();
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700)).unwrap();
        std::fs::remove_dir(root).unwrap();

        assert_eq!(result, Ok(()));
    }

    #[cfg(unix)]
    #[test]
    fn service_executable_rejects_symbolic_links() {
        let root = std::env::temp_dir()
            .canonicalize()
            .unwrap()
            .join(format!("ullage-exe-symlink-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let target = root.join("ullage");
        std::fs::write(&target, b"#!/bin/sh\n").unwrap();
        let link = root.join("linked");
        std::os::unix::fs::symlink(&target, &link).unwrap();

        let result = validate_executable(&link);

        std::fs::remove_file(&link).unwrap();
        std::fs::remove_file(&target).unwrap();
        std::fs::remove_dir(root).unwrap();

        assert!(result.is_err());
    }

    #[cfg(unix)]
    #[test]
    fn private_manifest_accepts_group_writable_mode() {
        use std::os::unix::fs::PermissionsExt;

        let root = std::env::temp_dir().canonicalize().unwrap().join(format!(
            "ullage-group-writable-manifest-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("ullage.service");
        std::fs::write(&path, b"[Unit]\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o664)).unwrap();

        let result = private_manifest_exists(&path);

        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        std::fs::remove_file(&path).unwrap();
        std::fs::remove_dir(root).unwrap();

        assert_eq!(result, Ok(true));
    }

    #[cfg(unix)]
    #[test]
    fn newly_created_service_directories_are_private() {
        use std::os::unix::fs::PermissionsExt;
        let root = std::env::temp_dir().canonicalize().unwrap().join(format!(
            "ullage-private-directories-test-{}",
            std::process::id()
        ));
        std::fs::create_dir(&root).unwrap();
        let parent = root.join("systemd/user");

        create_private_directories(&parent).unwrap();

        for directory in [root.join("systemd"), parent.clone()] {
            assert_eq!(
                std::fs::metadata(directory).unwrap().permissions().mode() & 0o777,
                0o700
            );
        }
        std::fs::remove_dir(parent).unwrap();
        std::fs::remove_dir(root.join("systemd")).unwrap();
        std::fs::remove_dir(root).unwrap();
    }
}
