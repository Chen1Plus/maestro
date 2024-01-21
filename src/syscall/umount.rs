//! The `umount` system call allows to unmount a filesystem previously mounted
//! with `mount`.

use crate::errno;
use crate::errno::Errno;
use crate::file::mountpoint;
use crate::file::path::Path;
use crate::process::mem_space::ptr::SyscallString;
use crate::process::Process;
use macros::syscall;

#[syscall]
pub fn umount(target: SyscallString) -> Result<i32, Errno> {
	let proc_mutex = Process::current_assert();
	let proc = proc_mutex.lock();

	let mem_space = proc.get_mem_space().unwrap();
	let mem_space_guard = mem_space.lock();
	let target_slice = target.get(&mem_space_guard)?.ok_or(errno!(EFAULT))?;
	let target_path = Path::new(target_slice)?;
	mountpoint::remove(&target_path)?;

	Ok(0)
}
