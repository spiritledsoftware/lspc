use std::{ffi::OsString, io, path::Path, process::Stdio, time::Duration};

use tokio::{
    process::{Child, ChildStderr, ChildStdin, ChildStdout, Command},
    time,
};

pub(crate) struct SupervisedServerProcess {
    child: Child,
    pid: u32,
    #[cfg(windows)]
    job: windows_sys::Win32::Foundation::HANDLE,
}

impl SupervisedServerProcess {
    pub(crate) fn spawn(
        executable: &Path,
        arguments: &[String],
    ) -> io::Result<(Self, ChildStdin, ChildStdout, ChildStderr)> {
        let mut command = Command::new(executable);
        command
            .args(arguments)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        #[cfg(unix)]
        {
            command.process_group(0);
        }
        let mut child = command.spawn()?;
        let pid = child
            .id()
            .ok_or_else(|| io::Error::other("Spawned server has no process identifier"))?;
        #[cfg(windows)]
        let job = create_kill_on_close_job(pid)?;
        let stdin = child.stdin.take().unwrap();
        let stdout = child.stdout.take().unwrap();
        let stderr = child.stderr.take().unwrap();
        Ok((
            Self {
                child,
                pid,
                #[cfg(windows)]
                job,
            },
            stdin,
            stdout,
            stderr,
        ))
    }

    pub(crate) fn pid(&self) -> u32 {
        self.pid
    }

    pub(crate) fn try_wait(&mut self) -> io::Result<Option<std::process::ExitStatus>> {
        self.child.try_wait()
    }

    pub(crate) async fn wait(&mut self) -> io::Result<std::process::ExitStatus> {
        self.child.wait().await
    }

    pub(crate) async fn terminate_process_tree(&mut self, grace: Duration) {
        if self.child.try_wait().ok().flatten().is_some() {
            return;
        }
        terminate_tree_soft(self.pid, self.platform_job());
        if time::timeout(grace, self.child.wait()).await.is_ok() {
            return;
        }
        terminate_tree_force(self.pid, self.platform_job());
        let _ = self.child.wait().await;
    }

    #[cfg(windows)]
    fn platform_job(&self) -> windows_sys::Win32::Foundation::HANDLE {
        self.job
    }

    #[cfg(not(windows))]
    fn platform_job(&self) {}
}

#[cfg(windows)]
impl Drop for SupervisedServerProcess {
    fn drop(&mut self) {
        unsafe {
            windows_sys::Win32::Foundation::CloseHandle(self.job);
        }
    }
}

#[cfg(unix)]
fn terminate_tree_soft(pid: u32, (): ()) {
    unsafe {
        libc::kill(-(pid as i32), libc::SIGTERM);
    }
}

#[cfg(unix)]
fn terminate_tree_force(pid: u32, (): ()) {
    unsafe {
        libc::kill(-(pid as i32), libc::SIGKILL);
    }
}

#[cfg(windows)]
fn terminate_tree_soft(_pid: u32, job: windows_sys::Win32::Foundation::HANDLE) {
    unsafe {
        windows_sys::Win32::System::JobObjects::TerminateJobObject(job, 1);
    }
}

#[cfg(windows)]
fn terminate_tree_force(_pid: u32, job: windows_sys::Win32::Foundation::HANDLE) {
    unsafe {
        windows_sys::Win32::System::JobObjects::TerminateJobObject(job, 1);
    }
}

#[cfg(not(any(unix, windows)))]
fn terminate_tree_soft(_pid: u32, (): ()) {}

#[cfg(not(any(unix, windows)))]
fn terminate_tree_force(_pid: u32, (): ()) {}

#[cfg(windows)]
fn create_kill_on_close_job(pid: u32) -> io::Result<windows_sys::Win32::Foundation::HANDLE> {
    use std::mem;
    use windows_sys::Win32::{
        Foundation::{CloseHandle, HANDLE},
        System::{
            JobObjects::{
                AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
                JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
                SetInformationJobObject,
            },
            Threading::{OpenProcess, PROCESS_SET_QUOTA, PROCESS_TERMINATE},
        },
    };

    unsafe {
        let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
        if job.is_null() {
            return Err(io::Error::last_os_error());
        }
        let mut information: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = mem::zeroed();
        information.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        if SetInformationJobObject(
            job,
            JobObjectExtendedLimitInformation,
            &information as *const _ as *const _,
            mem::size_of_val(&information) as u32,
        ) == 0
        {
            CloseHandle(job);
            return Err(io::Error::last_os_error());
        }
        let process: HANDLE = OpenProcess(PROCESS_SET_QUOTA | PROCESS_TERMINATE, 0, pid);
        if process.is_null() {
            CloseHandle(job);
            return Err(io::Error::last_os_error());
        }
        let assigned = AssignProcessToJobObject(job, process);
        CloseHandle(process);
        if assigned == 0 {
            CloseHandle(job);
            return Err(io::Error::last_os_error());
        }
        Ok(job)
    }
}

pub(crate) fn current_environment() -> Vec<(OsString, OsString)> {
    std::env::vars_os().collect()
}
