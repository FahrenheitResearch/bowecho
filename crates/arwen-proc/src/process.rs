// SPDX-License-Identifier: Apache-2.0
//
// ADOPTED VERBATIM from rusty-weather rw-sim/src/process.rs (Apache-2.0,
// branch codex/rwwps-simulation-studio @ 31b681e3) per the 2026-08-07
// ArWen Studio recon. Only this attribution header and the test env-var
// names differ.

//! Cross-platform ownership for external process trees launched by ArWen
//! Studio.
//!
//! Windows children are born suspended and assigned to a kill-on-close Job
//! Object before they can create descendants. POSIX children are placed in a
//! dedicated process group. Dropping the guard therefore closes the same
//! ownership boundary used by the desktop, CLI, and runtime probes.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};

/// Remove ambient user/Python state and install only explicit variables plus
/// the minimum platform environment required to execute system programs.
pub fn configure_isolated_child_environment(
    command: &mut Command,
    explicit: &BTreeMap<String, String>,
    private_runtime: Option<&Path>,
) -> Result<(), String> {
    command.env_clear().envs(explicit);
    if let Some(runtime) = private_runtime {
        fs::create_dir_all(runtime).map_err(|error| {
            format!(
                "could not create isolated child runtime {}: {error}",
                runtime.display()
            )
        })?;
    }
    #[cfg(windows)]
    {
        let system_directory = trusted_windows_system_directory()
            .ok_or_else(|| "could not resolve the Windows system directory".to_string())?;
        let windows_directory = system_directory
            .parent()
            .ok_or_else(|| "Windows system directory has no parent".to_string())?;
        command
            .env("SystemRoot", windows_directory)
            .env("WINDIR", windows_directory)
            .env("PATH", &system_directory)
            .env("PATHEXT", ".COM;.EXE;.BAT;.CMD")
            .env("ComSpec", system_directory.join("cmd.exe"));
        if let Ok(program_files) = trusted_program_files_directory() {
            command
                .env("ProgramFiles", &program_files)
                .env("ProgramW6432", program_files);
        }
        let runtime = match private_runtime {
            Some(runtime) => runtime.to_path_buf(),
            None => {
                let runtime = trusted_program_data_directory()?.join("gpuwm").join("tmp");
                fs::create_dir_all(&runtime).map_err(|error| {
                    format!(
                        "could not create shared isolated runtime {}: {error}",
                        runtime.display()
                    )
                })?;
                runtime
            }
        };
        let python_cache = runtime.join("PythonCache");
        fs::create_dir_all(&python_cache).map_err(|error| {
            format!(
                "could not create isolated Python cache {}: {error}",
                python_cache.display()
            )
        })?;
        command
            .env("TEMP", &runtime)
            .env("TMP", &runtime)
            .env("USERPROFILE", &runtime)
            .env("APPDATA", &runtime)
            .env("LOCALAPPDATA", &runtime)
            // `-B` and PYTHONDONTWRITEBYTECODE remain mandatory. This second
            // boundary also redirects a descendant or library that enables
            // bytecode writing, so no launch can mutate the sealed portable
            // interpreter tree.
            .env("PYTHONPYCACHEPREFIX", &python_cache)
            // Windows PowerShell can otherwise resolve its module-analysis
            // cache relative to the caller's working directory even when the
            // ordinary user-profile variables above are isolated.  Pinning
            // the cache file keeps a model launch from mutating a sealed app
            // bundle when that bundle is the caller's current directory.
            .env(
                "PSModuleAnalysisCachePath",
                runtime.join("PowerShell").join("ModuleAnalysisCache"),
            );
    }
    #[cfg(unix)]
    {
        command.env("PATH", "/usr/bin:/bin");
        if let Some(runtime) = private_runtime {
            let python_cache = runtime.join("PythonCache");
            fs::create_dir_all(&python_cache).map_err(|error| {
                format!(
                    "could not create isolated Python cache {}: {error}",
                    python_cache.display()
                )
            })?;
            command
                .env("HOME", runtime)
                .env("TMPDIR", runtime)
                .env("PYTHONPYCACHEPREFIX", python_cache);
        }
    }
    Ok(())
}

/// The directory the sealed child sees as its home (Windows:
/// `USERPROFILE`; POSIX: `HOME`), for callers that must stage a file the
/// child's libraries look up under `Path.home()` (e.g. `~/.cdsapirc`).
/// ADDITIVE to the adopted-verbatim sealer above and derived from the
/// same rules: the private runtime when one is set, else the shared
/// ProgramData runtime on Windows; POSIX children without a private
/// runtime inherit no HOME at all (`None`).
pub fn effective_child_home(private_runtime: Option<&Path>) -> Result<Option<PathBuf>, String> {
    if let Some(runtime) = private_runtime {
        return Ok(Some(runtime.to_path_buf()));
    }
    #[cfg(windows)]
    {
        Ok(Some(
            trusted_program_data_directory()?.join("gpuwm").join("tmp"),
        ))
    }
    #[cfg(not(windows))]
    {
        Ok(None)
    }
}

/// Configure a child so it cannot run ahead of the ownership guard.
pub fn prepare_owned_child(command: &mut Command) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        const CREATE_SUSPENDED: u32 = 0x0000_0004;
        command.creation_flags(CREATE_NO_WINDOW | CREATE_SUSPENDED);
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
}

/// Resume a Windows child after it has entered its Job Object. Other
/// platforms do not suspend the child and therefore need no action.
pub fn resume_owned_child(process_id: u32) -> Result<(), String> {
    #[cfg(windows)]
    {
        use std::mem::size_of;
        use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
        use windows_sys::Win32::System::Diagnostics::ToolHelp::{
            CreateToolhelp32Snapshot, TH32CS_SNAPTHREAD, THREADENTRY32, Thread32First, Thread32Next,
        };
        use windows_sys::Win32::System::Threading::{
            OpenThread, ResumeThread, THREAD_SUSPEND_RESUME,
        };

        let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0) };
        if snapshot == INVALID_HANDLE_VALUE {
            return Err(std::io::Error::last_os_error().to_string());
        }
        let mut entry = THREADENTRY32 {
            dwSize: size_of::<THREADENTRY32>() as u32,
            ..Default::default()
        };
        let mut found = false;
        let mut ok = unsafe { Thread32First(snapshot, &mut entry) };
        while ok != 0 {
            if entry.th32OwnerProcessID == process_id {
                let thread = unsafe { OpenThread(THREAD_SUSPEND_RESUME, 0, entry.th32ThreadID) };
                if thread.is_null() {
                    let error = std::io::Error::last_os_error().to_string();
                    unsafe { CloseHandle(snapshot) };
                    return Err(error);
                }
                let previous = unsafe { ResumeThread(thread) };
                unsafe { CloseHandle(thread) };
                if previous == u32::MAX {
                    let error = std::io::Error::last_os_error().to_string();
                    unsafe { CloseHandle(snapshot) };
                    return Err(error);
                }
                found = true;
                break;
            }
            ok = unsafe { Thread32Next(snapshot, &mut entry) };
        }
        unsafe { CloseHandle(snapshot) };
        if !found {
            return Err(format!(
                "no primary thread was found for suspended process {process_id}"
            ));
        }
    }
    #[cfg(not(windows))]
    let _ = process_id;
    Ok(())
}

/// Kill the owned tree and reap its direct child. Safe to call repeatedly.
pub fn terminate_and_reap(child: &mut Child, process_tree: &mut ProcessTreeGuard) {
    process_tree.terminate();
    let _ = child.kill();
    let _ = child.wait();
}

#[cfg(windows)]
pub struct ProcessTreeGuard {
    job: windows_sys::Win32::Foundation::HANDLE,
}

#[cfg(windows)]
impl ProcessTreeGuard {
    pub fn attach(child: &Child) -> Result<Self, String> {
        use std::mem::size_of;
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::Foundation::CloseHandle;
        use windows_sys::Win32::System::JobObjects::{
            AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
            JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
            SetInformationJobObject,
        };

        let job = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
        if job.is_null() {
            return Err(std::io::Error::last_os_error().to_string());
        }
        let mut information = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        information.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        let configured = unsafe {
            SetInformationJobObject(
                job,
                JobObjectExtendedLimitInformation,
                (&information as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
                size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            )
        };
        if configured == 0 {
            let error = std::io::Error::last_os_error().to_string();
            unsafe { CloseHandle(job) };
            return Err(error);
        }
        if unsafe { AssignProcessToJobObject(job, child.as_raw_handle()) } == 0 {
            let error = std::io::Error::last_os_error().to_string();
            unsafe { CloseHandle(job) };
            return Err(error);
        }
        Ok(Self { job })
    }

    pub fn terminate(&mut self) {
        use windows_sys::Win32::System::JobObjects::TerminateJobObject;
        unsafe { TerminateJobObject(self.job, 1) };
    }

    pub fn peak_memory_bytes(&self) -> Option<u64> {
        use std::mem::size_of;
        use windows_sys::Win32::System::JobObjects::{
            JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
            QueryInformationJobObject,
        };
        let mut information = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        let mut returned = 0u32;
        let ok = unsafe {
            QueryInformationJobObject(
                self.job,
                JobObjectExtendedLimitInformation,
                (&mut information as *mut JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
                size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
                &mut returned,
            )
        };
        (ok != 0).then_some(information.PeakJobMemoryUsed as u64)
    }

    pub fn memory_scope(&self) -> &'static str {
        "Windows Job Object peak committed memory across assigned processes"
    }
}

#[cfg(windows)]
impl Drop for ProcessTreeGuard {
    fn drop(&mut self) {
        use windows_sys::Win32::Foundation::CloseHandle;
        unsafe { CloseHandle(self.job) };
    }
}

#[cfg(unix)]
pub struct ProcessTreeGuard {
    process_group: i32,
    terminated: bool,
}

#[cfg(unix)]
impl ProcessTreeGuard {
    pub fn attach(child: &Child) -> Result<Self, String> {
        let process_group = i32::try_from(child.id())
            .map_err(|_| "child PID does not fit a POSIX process-group id".to_string())?;
        Ok(Self {
            process_group,
            terminated: false,
        })
    }

    pub fn terminate(&mut self) {
        if !self.terminated {
            unsafe { libc::kill(-self.process_group, libc::SIGKILL) };
            self.terminated = true;
        }
    }

    pub fn peak_memory_bytes(&self) -> Option<u64> {
        None
    }

    pub fn memory_scope(&self) -> &'static str {
        "process group owned; aggregate peak memory unavailable on this build"
    }
}

#[cfg(unix)]
impl Drop for ProcessTreeGuard {
    fn drop(&mut self) {
        self.terminate();
    }
}

#[cfg(not(any(windows, unix)))]
pub struct ProcessTreeGuard;

#[cfg(not(any(windows, unix)))]
impl ProcessTreeGuard {
    pub fn attach(_child: &Child) -> Result<Self, String> {
        Ok(Self)
    }
    pub fn terminate(&mut self) {}
    pub fn peak_memory_bytes(&self) -> Option<u64> {
        None
    }
    pub fn memory_scope(&self) -> &'static str {
        "process-tree accounting unavailable on this platform"
    }
}

#[cfg(windows)]
pub(crate) fn trusted_program_data_directory() -> Result<PathBuf, String> {
    use windows_sys::Win32::UI::Shell::FOLDERID_ProgramData;
    trusted_known_folder(&FOLDERID_ProgramData, "ProgramData")
}

#[cfg(windows)]
fn trusted_program_files_directory() -> Result<PathBuf, String> {
    use windows_sys::Win32::UI::Shell::FOLDERID_ProgramFiles;
    trusted_known_folder(&FOLDERID_ProgramFiles, "Program Files")
}

#[cfg(windows)]
fn trusted_known_folder(
    folder_id: &windows_sys::core::GUID,
    label: &str,
) -> Result<PathBuf, String> {
    use std::os::windows::ffi::OsStringExt;
    use windows_sys::Win32::System::Com::CoTaskMemFree;
    use windows_sys::Win32::UI::Shell::SHGetKnownFolderPath;

    let mut wide: *mut u16 = std::ptr::null_mut();
    let result = unsafe { SHGetKnownFolderPath(folder_id, 0, std::ptr::null_mut(), &mut wide) };
    if result < 0 || wide.is_null() {
        return Err(format!(
            "could not resolve {label} (HRESULT 0x{:08x})",
            result as u32
        ));
    }
    let length = unsafe {
        let mut length = 0usize;
        while *wide.add(length) != 0 {
            length += 1;
        }
        length
    };
    let path = PathBuf::from(std::ffi::OsString::from_wide(unsafe {
        std::slice::from_raw_parts(wide, length)
    }));
    unsafe { CoTaskMemFree(wide.cast()) };
    let metadata = fs::symlink_metadata(&path)
        .map_err(|error| format!("{label} is unavailable {}: {error}", path.display()))?;
    use std::os::windows::fs::MetadataExt;
    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0400;
    if metadata.file_type().is_symlink()
        || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
    {
        return Err(format!(
            "{label} must not be a reparse/symlink root: {}",
            path.display()
        ));
    }
    let canonical = fs::canonicalize(&path)
        .map_err(|error| format!("could not canonicalize {label} {}: {error}", path.display()))?;
    Ok(strip_verbatim_prefix(canonical))
}

/// `fs::canonicalize` returns `\\?\C:\...` extended-length paths. A child
/// that joins one with a forward slash (CuPy's `~/.cupy`, observed live:
/// `\\?\C:\ProgramData\gpuwm\tmp/.cupy` → WinError 123) breaks, because
/// verbatim paths accept backslashes only. Environment values handed to
/// sealed children therefore use the plain drive form. (Deviation from
/// the adopted rw-sim file, found live-firing gpuwm through the sealer.)
#[cfg(windows)]
fn strip_verbatim_prefix(path: PathBuf) -> PathBuf {
    let text = path.to_string_lossy();
    match text.strip_prefix(r"\\?\") {
        Some(stripped) if !stripped.starts_with("UNC") => PathBuf::from(stripped),
        _ => path,
    }
}

#[cfg(windows)]
fn trusted_windows_system_directory() -> Option<PathBuf> {
    use std::os::windows::ffi::OsStringExt;

    unsafe extern "system" {
        fn GetSystemDirectoryW(buffer: *mut u16, size: u32) -> u32;
    }
    let mut buffer = vec![0u16; 32_768];
    let length = unsafe { GetSystemDirectoryW(buffer.as_mut_ptr(), buffer.len() as u32) } as usize;
    if length == 0 || length >= buffer.len() {
        return None;
    }
    buffer.truncate(length);
    Some(PathBuf::from(std::ffi::OsString::from_wide(&buffer)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Stdio;
    use std::time::{Duration, Instant};

    const HELPER_ENV: &str = "ARWEN_PROC_PROCESS_TREE_PARENT_HELPER";
    const PID_FILE_ENV: &str = "ARWEN_PROC_PROCESS_TREE_DESCENDANT_PID_FILE";

    #[test]
    fn isolated_child_routes_user_caches_into_its_private_runtime() {
        let root = std::env::temp_dir().join(format!(
            "rw-sim-isolated-environment-{}-{}",
            std::process::id(),
            chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default()
        ));
        let runtime = root.join("runtime");
        let mut command = Command::new("unused-program");
        configure_isolated_child_environment(&mut command, &BTreeMap::new(), Some(&runtime))
            .unwrap();
        let env_path = |key: &str| {
            PathBuf::from(
                command
                    .get_envs()
                    .find(|(name, _)| *name == key)
                    .and_then(|(_, value)| value)
                    .unwrap(),
            )
        };
        #[cfg(windows)]
        for key in ["TEMP", "TMP", "USERPROFILE", "APPDATA", "LOCALAPPDATA"] {
            assert_eq!(env_path(key), runtime, "{key}");
        }
        #[cfg(windows)]
        assert_eq!(
            env_path("PSModuleAnalysisCachePath"),
            runtime.join("PowerShell").join("ModuleAnalysisCache")
        );
        #[cfg(unix)]
        for key in ["HOME", "TMPDIR"] {
            assert_eq!(env_path(key), runtime, "{key}");
        }
        assert_eq!(env_path("PYTHONPYCACHEPREFIX"), runtime.join("PythonCache"));
        fs::remove_dir_all(root).unwrap();
    }

    #[cfg(windows)]
    #[test]
    fn powershell_module_analysis_cannot_mutate_the_callers_directory() {
        let root = std::env::temp_dir().join(format!(
            "rw-sim-powershell-cache-{}-{}",
            std::process::id(),
            chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default()
        ));
        let caller = root.join("sealed-package");
        let runtime = root.join("private-runtime");
        fs::create_dir_all(&caller).unwrap();

        let powershell = trusted_windows_system_directory()
            .unwrap()
            .join("WindowsPowerShell")
            .join("v1.0")
            .join("powershell.exe");
        let mut command = Command::new(powershell);
        command
            .args([
                "-NoProfile",
                "-NonInteractive",
                "-Command",
                "$p=[Environment]::GetEnvironmentVariable('PSModuleAnalysisCachePath'); Get-Module -ListAvailable | Out-Null; [Console]::Out.Write($p)",
            ])
            .current_dir(&caller)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        configure_isolated_child_environment(&mut command, &BTreeMap::new(), Some(&runtime))
            .unwrap();
        let output = command.output().unwrap();
        assert!(
            output.status.success(),
            "PowerShell failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(
            PathBuf::from(String::from_utf8(output.stdout).unwrap()),
            runtime.join("PowerShell").join("ModuleAnalysisCache")
        );
        assert!(
            !caller.join("Microsoft").exists(),
            "PowerShell created a module cache inside the sealed caller directory"
        );

        fs::remove_dir_all(root).unwrap();
    }

    /// Invoked only as an owned subprocess by the test below. The ignored
    /// marker prevents the ordinary test harness from starting a sleeper.
    #[test]
    #[ignore = "subprocess helper"]
    fn process_tree_parent_helper() {
        if std::env::var(HELPER_ENV).as_deref() != Ok("1") {
            return;
        }
        let pid_file = PathBuf::from(std::env::var_os(PID_FILE_ENV).expect("PID file env"));
        #[cfg(windows)]
        let mut descendant = Command::new("cmd.exe")
            .args(["/D", "/S", "/C", "ping -n 120 127.0.0.1 >NUL"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn Windows descendant");
        #[cfg(unix)]
        let mut descendant = Command::new("/bin/sh")
            .args(["-c", "sleep 120"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn POSIX descendant");
        fs::write(&pid_file, descendant.id().to_string()).expect("publish descendant PID");
        let _ = descendant.wait();
    }

    #[test]
    fn process_tree_guard_drop_terminates_a_descendant() {
        let root = std::env::temp_dir().join(format!(
            "rw-sim-process-tree-{}-{}",
            std::process::id(),
            chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default()
        ));
        fs::create_dir_all(&root).unwrap();
        let pid_file = root.join("descendant.pid");
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .args([
                "--ignored",
                "--exact",
                "process::tests::process_tree_parent_helper",
            ])
            .env(HELPER_ENV, "1")
            .env(PID_FILE_ENV, &pid_file)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        prepare_owned_child(&mut command);
        let mut child = command.spawn().unwrap();
        let mut guard = ProcessTreeGuard::attach(&child).unwrap_or_else(|error| {
            let _ = child.kill();
            let _ = child.wait();
            panic!("attach process-tree guard: {error}")
        });
        if let Err(error) = resume_owned_child(child.id()) {
            terminate_and_reap(&mut child, &mut guard);
            panic!("resume owned helper: {error}")
        }
        let started = Instant::now();
        while !pid_file.is_file() && started.elapsed() < Duration::from_secs(10) {
            std::thread::sleep(Duration::from_millis(20));
        }
        let descendant_pid = fs::read_to_string(&pid_file)
            .expect("helper published descendant PID")
            .parse::<u32>()
            .expect("descendant PID is numeric");

        #[cfg(windows)]
        let descendant_handle = {
            use windows_sys::Win32::System::Threading::OpenProcess;
            const SYNCHRONIZE: u32 = 0x0010_0000;
            let handle = unsafe { OpenProcess(SYNCHRONIZE, 0, descendant_pid) };
            assert!(!handle.is_null(), "open descendant process handle");
            handle
        };

        // The destructor itself is the abnormal-return safety boundary.
        drop(guard);
        let reaped = Instant::now();
        loop {
            if child.try_wait().unwrap().is_some() {
                break;
            }
            assert!(
                reaped.elapsed() < Duration::from_secs(5),
                "owned direct child survived guard drop"
            );
            std::thread::sleep(Duration::from_millis(20));
        }

        #[cfg(windows)]
        {
            use windows_sys::Win32::Foundation::{CloseHandle, WAIT_OBJECT_0};
            use windows_sys::Win32::System::Threading::WaitForSingleObject;
            let result = unsafe { WaitForSingleObject(descendant_handle, 5_000) };
            unsafe { CloseHandle(descendant_handle) };
            assert_eq!(result, WAIT_OBJECT_0, "descendant survived Job close");
        }
        #[cfg(unix)]
        {
            let started = Instant::now();
            loop {
                let alive = unsafe { libc::kill(descendant_pid as i32, 0) } == 0;
                if !alive {
                    break;
                }
                assert!(
                    started.elapsed() < Duration::from_secs(5),
                    "descendant survived process-group guard drop"
                );
                std::thread::sleep(Duration::from_millis(20));
            }
        }
        fs::remove_dir_all(root).unwrap();
    }
}
