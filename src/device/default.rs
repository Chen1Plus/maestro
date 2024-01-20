/*
 * Copyright 2024 Luc Lenôtre
 *
 * This file is part of Maestro.
 *
 * Maestro is free software: you can redistribute it and/or modify it under the
 * terms of the GNU General Public License as published by the Free Software
 * Foundation, either version 3 of the License, or (at your option) any later
 * version.
 *
 * Maestro is distributed in the hope that it will be useful, but WITHOUT ANY
 * WARRANTY; without even the implied warranty of MERCHANTABILITY or FITNESS FOR
 * A PARTICULAR PURPOSE. See the GNU General Public License for more details.
 *
 * You should have received a copy of the GNU General Public License along with
 * Maestro. If not, see <https://www.gnu.org/licenses/>.
 */

//! This module implements default devices.

use super::id;
use super::DeviceType;
use crate::crypto::rand;
use crate::device;
use crate::device::tty::TTYDeviceHandle;
use crate::device::Device;
use crate::device::DeviceHandle;
use crate::device::DeviceID;
use crate::errno;
use crate::errno::EResult;
use crate::errno::Errno;
use crate::file::blocking::BlockHandler;
use crate::file::path::Path;
use crate::logger::LOGGER;
use crate::process::mem_space::MemSpace;
use crate::process::Process;
use crate::syscall::ioctl;
use crate::util::io;
use crate::util::io::IO;
use crate::util::lock::IntMutex;
use crate::util::ptr::arc::Arc;
use core::cmp::min;
use core::ffi::c_void;
use core::mem::ManuallyDrop;

/// Device which does nothing.
#[derive(Default)]
pub struct NullDeviceHandle {}

impl DeviceHandle for NullDeviceHandle {
	fn ioctl(
		&mut self,
		_mem_space: Arc<IntMutex<MemSpace>>,
		_request: ioctl::Request,
		_argp: *const c_void,
	) -> Result<u32, Errno> {
		// TODO
		Err(errno!(EINVAL))
	}
}

impl IO for NullDeviceHandle {
	fn get_size(&self) -> u64 {
		0
	}

	fn read(&mut self, _offset: u64, _buff: &mut [u8]) -> Result<(u64, bool), Errno> {
		Ok((0, true))
	}

	fn write(&mut self, _offset: u64, buff: &[u8]) -> Result<u64, Errno> {
		Ok(buff.len() as _)
	}

	fn poll(&mut self, _mask: u32) -> Result<u32, Errno> {
		Ok(io::POLLIN | io::POLLOUT)
	}
}

/// Device returning only null bytes.
#[derive(Default)]
pub struct ZeroDeviceHandle {}

impl DeviceHandle for ZeroDeviceHandle {
	fn ioctl(
		&mut self,
		_mem_space: Arc<IntMutex<MemSpace>>,
		_request: ioctl::Request,
		_argp: *const c_void,
	) -> Result<u32, Errno> {
		// TODO
		Err(errno!(EINVAL))
	}
}

impl IO for ZeroDeviceHandle {
	fn get_size(&self) -> u64 {
		0
	}

	fn read(&mut self, _offset: u64, buff: &mut [u8]) -> Result<(u64, bool), Errno> {
		for b in buff.iter_mut() {
			*b = 0;
		}

		Ok((buff.len() as _, false))
	}

	fn write(&mut self, _offset: u64, buff: &[u8]) -> Result<u64, Errno> {
		Ok(buff.len() as _)
	}

	fn poll(&mut self, _mask: u32) -> Result<u32, Errno> {
		Ok(io::POLLIN | io::POLLOUT)
	}
}

/// Device allows to get random bytes.
///
/// This device will block reading until enough entropy is available.
#[derive(Default)]
pub struct RandomDeviceHandle {
	/// The device's block handler.
	block_handler: BlockHandler,
}

impl DeviceHandle for RandomDeviceHandle {
	fn ioctl(
		&mut self,
		_mem_space: Arc<IntMutex<MemSpace>>,
		_request: ioctl::Request,
		_argp: *const c_void,
	) -> Result<u32, Errno> {
		// TODO
		Err(errno!(EINVAL))
	}

	fn add_waiting_process(&mut self, proc: &mut Process, mask: u32) -> Result<(), Errno> {
		self.block_handler.add_waiting_process(proc, mask)
	}
}

impl IO for RandomDeviceHandle {
	fn get_size(&self) -> u64 {
		0
	}

	fn read(&mut self, _: u64, buff: &mut [u8]) -> Result<(u64, bool), Errno> {
		let mut pool = rand::ENTROPY_POOL.lock();

		self.block_handler.wake_processes(io::POLLIN);

		if let Some(pool) = &mut *pool {
			let len = pool.read(buff, false);
			Ok((len as _, false))
		} else {
			Ok((0, true))
		}
	}

	fn write(&mut self, _: u64, buff: &[u8]) -> Result<u64, Errno> {
		let mut pool = rand::ENTROPY_POOL.lock();

		self.block_handler.wake_processes(io::POLLOUT);

		if let Some(pool) = &mut *pool {
			let len = pool.write(buff);
			Ok(len as _)
		} else {
			Err(errno!(EINVAL))
		}
	}

	fn poll(&mut self, _mask: u32) -> Result<u32, Errno> {
		Ok(io::POLLIN | io::POLLOUT)
	}
}

/// This device works exactly like [`RandomDeviceHandle`], except it doesn't block.
///
/// If not enough entropy is available, the output might not have a sufficient
/// quality.
#[derive(Default)]
pub struct URandomDeviceHandle {}

impl DeviceHandle for URandomDeviceHandle {
	fn ioctl(
		&mut self,
		_mem_space: Arc<IntMutex<MemSpace>>,
		_request: ioctl::Request,
		_argp: *const c_void,
	) -> Result<u32, Errno> {
		// TODO
		Err(errno!(EINVAL))
	}
}

impl IO for URandomDeviceHandle {
	fn get_size(&self) -> u64 {
		0
	}

	fn read(&mut self, _: u64, buff: &mut [u8]) -> Result<(u64, bool), Errno> {
		let mut pool = rand::ENTROPY_POOL.lock();

		if let Some(pool) = &mut *pool {
			let len = pool.read(buff, true);
			Ok((len as _, false))
		} else {
			Ok((0, true))
		}
	}

	fn write(&mut self, _: u64, buff: &[u8]) -> Result<u64, Errno> {
		let mut pool = rand::ENTROPY_POOL.lock();

		if let Some(pool) = &mut *pool {
			let len = pool.write(buff);
			Ok(len as _)
		} else {
			Err(errno!(EINVAL))
		}
	}

	fn poll(&mut self, _mask: u32) -> Result<u32, Errno> {
		Ok(io::POLLIN | io::POLLOUT)
	}
}

/// Device allowing to read or write kernel logs.
#[derive(Default)]
pub struct KMsgDeviceHandle {}

impl DeviceHandle for KMsgDeviceHandle {
	fn ioctl(
		&mut self,
		_mem_space: Arc<IntMutex<MemSpace>>,
		_request: ioctl::Request,
		_argp: *const c_void,
	) -> Result<u32, Errno> {
		// TODO
		Err(errno!(EINVAL))
	}
}

impl IO for KMsgDeviceHandle {
	fn get_size(&self) -> u64 {
		LOGGER.lock().get_size() as _
	}

	fn read(&mut self, offset: u64, buff: &mut [u8]) -> Result<(u64, bool), Errno> {
		if offset > (usize::MAX as u64) {
			return Err(errno!(EINVAL));
		}

		let logger = LOGGER.lock();
		let size = logger.get_size();
		let content = logger.get_content();

		let len = min(size - offset as usize, buff.len());
		buff[..len].copy_from_slice(&content[(offset as usize)..(offset as usize + len)]);

		let eof = offset as usize + len >= size;
		Ok((len as _, eof))
	}

	fn write(&mut self, _offset: u64, _buff: &[u8]) -> Result<u64, Errno> {
		// TODO
		todo!();
	}

	fn poll(&mut self, _mask: u32) -> Result<u32, Errno> {
		Ok(io::POLLIN | io::POLLOUT)
	}
}

/// Creates the default devices.
pub(super) fn create() -> EResult<()> {
	let _first_major = ManuallyDrop::new(id::alloc_major(DeviceType::Char, Some(1))?);

	let null_path = Path::from_str(b"/dev/null", false)?;
	let null_device = Device::new(
		DeviceID {
			type_: DeviceType::Char,
			major: 1,
			minor: 3,
		},
		null_path,
		0o666,
		NullDeviceHandle::default(),
	)?;
	device::register(null_device)?;

	let zero_path = Path::from_str(b"/dev/zero", false)?;
	let zero_device = Device::new(
		DeviceID {
			type_: DeviceType::Char,
			major: 1,
			minor: 5,
		},
		zero_path,
		0o666,
		ZeroDeviceHandle::default(),
	)?;
	device::register(zero_device)?;

	let random_path = Path::from_str(b"/dev/random", false)?;
	let random_device = Device::new(
		DeviceID {
			type_: DeviceType::Char,
			major: 1,
			minor: 8,
		},
		random_path,
		0o666,
		RandomDeviceHandle::default(),
	)?;
	device::register(random_device)?;

	let urandom_path = Path::from_str(b"/dev/urandom", false)?;
	let urandom_device = Device::new(
		DeviceID {
			type_: DeviceType::Char,
			major: 1,
			minor: 9,
		},
		urandom_path,
		0o666,
		URandomDeviceHandle::default(),
	)?;
	device::register(urandom_device)?;

	let kmsg_path = Path::from_str(b"/dev/kmsg", false)?;
	let kmsg_device = Device::new(
		DeviceID {
			type_: DeviceType::Char,
			major: 1,
			minor: 11,
		},
		kmsg_path,
		0o600,
		KMsgDeviceHandle::default(),
	)?;
	device::register(kmsg_device)?;

	let _fifth_major = ManuallyDrop::new(id::alloc_major(DeviceType::Char, Some(5))?);

	let current_tty_path = Path::from_str(b"/dev/tty", false)?;
	let current_tty_device = Device::new(
		DeviceID {
			type_: DeviceType::Char,
			major: 5,
			minor: 0,
		},
		current_tty_path,
		0o666,
		TTYDeviceHandle::new(None),
	)?;
	device::register(current_tty_device)?;

	Ok(())
}
