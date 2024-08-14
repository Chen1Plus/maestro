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

//! A pipe is an object that links two file descriptors together. One reading
//! and another writing, with a buffer in between.

use super::Buffer;
use crate::{
	file::{buffer::WaitQueue, fs::NodeOps, FileLocation, FileType, Stat},
	limits,
	process::{mem_space::copy::SyscallPtr, signal::Signal, Process},
	syscall::{ioctl, FromSyscallArg},
};
use core::{
	ffi::{c_int, c_void},
	intrinsics::unlikely,
};
use utils::{
	collections::{ring_buffer::RingBuffer, vec::Vec},
	errno,
	errno::EResult,
	lock::Mutex,
	vec, TryDefault,
};

/// Representing a FIFO buffer.
#[derive(Debug)]
pub struct PipeBuffer {
	/// The pipe's buffer.
	buffer: Mutex<RingBuffer<u8, Vec<u8>>>,

	/// The number of readers on the pipe.
	readers: usize,
	/// The number of writers on the pipe.
	writers: usize,

	/// The queue of processing waiting to read from the pipe.
	rd_queue: WaitQueue,
	/// The queue of processing waiting to write to the pipe.
	wr_queue: WaitQueue,
}

impl PipeBuffer {
	/// Returns the capacity of the pipe in bytes.
	pub fn get_capacity(&self) -> usize {
		self.buffer.lock().get_size()
	}

	/// Returns the available space in the buffer in bytes.
	pub fn get_available_len(&self) -> usize {
		self.buffer.lock().get_available_len()
	}
}

impl TryDefault for PipeBuffer {
	fn try_default() -> Result<Self, Self::Error> {
		Ok(Self {
			buffer: Mutex::new(RingBuffer::new(vec![0; limits::PIPE_BUF]?)),

			readers: 0,
			writers: 0,

			rd_queue: WaitQueue::default(),
			wr_queue: WaitQueue::default(),
		})
	}
}

impl Buffer for PipeBuffer {
	fn acquire(&self, read: bool, write: bool) {
		if read {
			self.readers += 1;
		}
		if write {
			self.writers += 1;
		}
	}

	fn release(&self, read: bool, write: bool) {
		if read {
			self.readers -= 1;
		}
		if write {
			self.writers -= 1;
		}
		if (self.readers == 0) != (self.writers == 0) {
			self.rd_queue.wake_all();
			self.wr_queue.wake_all();
		}
	}
}

impl NodeOps for PipeBuffer {
	fn get_stat(&self, _loc: &FileLocation) -> EResult<Stat> {
		Ok(Stat {
			file_type: FileType::Fifo,
			mode: 0o666,
			..Default::default()
		})
	}

	fn ioctl(
		&self,
		_loc: &FileLocation,
		request: ioctl::Request,
		argp: *const c_void,
	) -> EResult<u32> {
		match request.get_old_format() {
			ioctl::FIONREAD => {
				let count_ptr = SyscallPtr::<c_int>::from_syscall_arg(argp as usize);
				count_ptr.copy_to_user(self.get_available_len() as _)?;
			}
			_ => return Err(errno!(ENOTTY)),
		}
		Ok(0)
	}

	fn read_content(&self, _loc: &FileLocation, _off: u64, buf: &mut [u8]) -> EResult<usize> {
		let len = self.buffer.lock().read(buf);
		if len > 0 {
			self.wr_queue.wake_next();
		}
		Ok(len)
	}

	fn write_content(&self, _loc: &FileLocation, _off: u64, buf: &[u8]) -> EResult<usize> {
		if unlikely(buf.len() == 0) {
			return Ok(0);
		}
		if self.readers == 0 {
			Process::current().lock().kill(Signal::SIGPIPE);
			return Err(errno!(EPIPE));
		}
		let len = self.buffer.lock().write(buf);
		self.rd_queue.wake_next();
		Ok(len)
	}
}
