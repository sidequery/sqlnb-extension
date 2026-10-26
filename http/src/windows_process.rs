//! A Git child enters its kill-on-close job before its first instruction runs.
use std::io;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::process::Child;
use windows_sys::Win32::{
    Foundation::*,
    System::{Diagnostics::ToolHelp::*, JobObjects::*, Threading::*, IO::CancelSynchronousIo},
};

pub(crate) struct Job(OwnedHandle);

impl Job {
    pub(crate) fn contain_and_resume(child: &mut Child) -> io::Result<Self> {
        let result = (|| unsafe {
            let handle = CreateJobObjectW(std::ptr::null(), std::ptr::null());
            if handle.is_null() {
                return Err(io::Error::last_os_error());
            }
            let job = Self(OwnedHandle::from_raw_handle(handle));
            let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
            limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            if SetInformationJobObject(
                handle,
                JobObjectExtendedLimitInformation,
                (&limits as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
                std::mem::size_of_val(&limits) as u32,
            ) == 0
                || AssignProcessToJobObject(handle, child.as_raw_handle()) == 0
            {
                return Err(io::Error::last_os_error());
            }
            // std::process owns the process handle but not its primary thread.
            // The suspended process has exactly its initial thread; find it
            // through the documented Toolhelp API and resume only after assignment.
            let snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0);
            if snapshot == INVALID_HANDLE_VALUE {
                return Err(io::Error::last_os_error());
            }
            let snapshot = OwnedHandle::from_raw_handle(snapshot);
            let mut entry = THREADENTRY32 {
                dwSize: std::mem::size_of::<THREADENTRY32>() as u32,
                ..Default::default()
            };
            let mut exists = Thread32First(snapshot.as_raw_handle(), &mut entry);
            while exists != 0 {
                if entry.th32OwnerProcessID == child.id() {
                    let thread = OpenThread(THREAD_SUSPEND_RESUME, 0, entry.th32ThreadID);
                    if thread.is_null() {
                        return Err(io::Error::last_os_error());
                    }
                    let thread = OwnedHandle::from_raw_handle(thread);
                    if ResumeThread(thread.as_raw_handle()) == u32::MAX {
                        return Err(io::Error::last_os_error());
                    }
                    return Ok(job);
                }
                exists = Thread32Next(snapshot.as_raw_handle(), &mut entry);
            }
            Err(io::Error::other("cannot find suspended Git thread"))
        })();
        if result.is_err() {
            let _ = child.kill();
            let _ = child.wait();
        }
        result
    }

    pub(crate) fn terminate(&self) {
        unsafe {
            TerminateJobObject(self.0.as_raw_handle(), 1);
        }
    }
}

pub(crate) fn cancel_pipe_worker<T>(worker: &std::thread::JoinHandle<T>) {
    // Completion is set before this call; a pending synchronous read/write
    // returns ERROR_OPERATION_ABORTED and the worker exits without retrying.
    unsafe {
        CancelSynchronousIo(worker.as_raw_handle());
    }
}
