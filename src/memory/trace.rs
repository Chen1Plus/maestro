//! Memory usage tracing utility functions.

use crate::{debug, device::serial, register_get};
use core::{ffi::c_void, ptr::null_mut};

/// Writes a memory tracing sample to the **COM2** serial port.
///
/// Arguments:
/// - `allocator` is the name of the allocator.
/// - `op` is the operation number.
/// - `ptr` is the affected pointer.
/// - `size` is the new size of the allocation. The unit is dependent on the allocator.
pub fn sample(allocator: &str, op: u8, ptr: *const c_void, size: usize) {
	// Dump callstack
	let mut callstack: [*mut c_void; 64] = [null_mut(); 64];
	unsafe {
		let esp = register_get!("esp");
		debug::get_callstack(esp as _, &mut callstack);
	}
	// COM2
	let mut serial = serial::PORTS[1].lock();
	// Write name of allocator
	serial.write(&(allocator.len() as u64).to_le_bytes());
	serial.write(allocator.as_bytes());
	// Write op
	serial.write(&[op]);
	// Write ptr and size
	serial.write(&(ptr as u64).to_le_bytes());
	serial.write(&(size as u64).to_le_bytes());
	// Write callstack
	let len = callstack
		.iter()
		.enumerate()
		.find(|(_, p)| p.is_null())
		.map(|(i, _)| i)
		.unwrap_or(callstack.len());
	serial.write(&[len as u8]);
	for f in &callstack[..len] {
		serial.write(&(*f as u64).to_le_bytes());
	}
}
