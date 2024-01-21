//! This module implements utility functions for files manipulations.

use super::path::{Component, Path, PathBuf};
use super::File;
use super::FileContent;
use crate::errno::EResult;
use crate::file::perm::AccessProfile;
use crate::file::vfs;
use crate::util::container::hashmap::HashMap;
use crate::util::container::string::String;
use crate::util::io::IO;
use crate::util::TryClone;
use crate::{errno, memory};

/// Creates the directories necessary to reach path `path`.
///
/// If relative, the path is taken from the root.
pub fn create_dirs(path: &Path) -> EResult<()> {
	// Path of the parent directory
	let mut p = PathBuf::root();
	for comp in path.components() {
		let Component::Normal(name) = &comp else {
			continue;
		};
		if let Ok(parent_mutex) = vfs::get_file_from_path(&p, &AccessProfile::KERNEL, true) {
			let mut parent = parent_mutex.lock();
			let res = vfs::create_file(
				&mut parent,
				String::try_from(*name)?,
				&AccessProfile::KERNEL,
				0o755,
				FileContent::Directory(HashMap::new()),
			);
			match res {
				Err(e) if e.as_int() != errno::EEXIST => return Err(e),
				_ => {}
			}
		}
		p.join(comp)?;
	}
	Ok(())
}

/// Copies the file `old` into the directory `new_parent` with name `new_name`.
pub fn copy_file(old: &mut File, new_parent: &mut File, new_name: String) -> EResult<()> {
	let ap = AccessProfile::from_file(old);
	let mode = old.get_mode();

	match old.get_content() {
		// Copy the file and its content
		FileContent::Regular => {
			let new_mutex =
				vfs::create_file(new_parent, new_name, &ap, mode, FileContent::Regular)?;
			let mut new = new_mutex.lock();

			// TODO On fail, remove file
			// Copying content
			let mut off = 0;
			let mut buff: [u8; memory::PAGE_SIZE] = [0; memory::PAGE_SIZE];
			loop {
				let (len, eof) = old.read(off, &mut buff)?;
				if eof {
					break;
				}

				new.write(off, &buff)?;
				off += len;
			}
		}

		// Copy the directory recursively
		FileContent::Directory(entries) => {
			let new_mutex = vfs::create_file(
				new_parent,
				new_name,
				&ap,
				mode,
				FileContent::Directory(HashMap::new()),
			)?;
			let mut new = new_mutex.lock();

			// TODO On fail, undo
			for (name, _) in entries.iter() {
				let old_mutex = vfs::get_file_from_parent(&new, name.try_clone()?, &ap, false)?;
				let mut old = old_mutex.lock();

				copy_file(&mut old, &mut new, name.try_clone()?)?;
			}
		}

		// Copy the file
		content => {
			vfs::create_file(new_parent, new_name, &ap, mode, content.try_clone()?)?;
		}
	}

	Ok(())
}

/// Removes the file `file` and its subfiles recursively if it's a directory.
///
/// Arguments:
/// - `file` is the root file to remove
/// - `access_profile` is the access profile, to check permissions
pub fn remove_recursive(file: &mut File, access_profile: &AccessProfile) -> EResult<()> {
	match file.get_content() {
		FileContent::Directory(entries) => {
			for (name, _) in entries.iter() {
				let name = name.try_clone()?;
				let subfile_mutex = vfs::get_file_from_parent(file, name, access_profile, false)?;
				let mut subfile = subfile_mutex.lock();

				remove_recursive(&mut subfile, access_profile)?;
			}
		}

		_ => vfs::remove_file(file, access_profile)?,
	}

	Ok(())
}
