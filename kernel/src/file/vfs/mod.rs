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

//! The VFS (Virtual FileSystem) aggregates every mounted filesystems into one.
//!
//! To manipulate files, the VFS should be used instead of
//! calling the filesystems' directly.

pub mod mountpoint;
pub mod node;

use super::{
	perm,
	perm::{AccessProfile, S_ISVTX},
	File, FileLocation, FileType, Stat,
};
use crate::{
	device,
	device::DeviceID,
	file::vfs::mountpoint::MountPoint,
	process::Process,
	sync::{mutex::Mutex, once::OnceInit},
	syscall::ioctl::Request,
};
use core::{
	borrow::Borrow,
	ffi::c_void,
	hash::{Hash, Hasher},
	intrinsics::unlikely,
};
use node::Node;
use utils::{
	collections::{
		hashmap::HashSet,
		path::{Component, Path, PathBuf},
		string::String,
		vec::Vec,
	},
	errno,
	errno::EResult,
	limits::{LINK_MAX, PATH_MAX, SYMLOOP_MAX},
	ptr::arc::Arc,
	vec,
};

/// A child of a VFS entry.
///
/// The [`Hash`] and [`PartialEq`] traits are forwarded to the entry's name.
#[derive(Debug)]
struct EntryChild(Arc<Entry>);

impl Borrow<[u8]> for EntryChild {
	fn borrow(&self) -> &[u8] {
		&self.0.name
	}
}

impl Eq for EntryChild {}

impl PartialEq for EntryChild {
	fn eq(&self, other: &Self) -> bool {
		self.0.name.eq(&other.0.name)
	}
}

impl Hash for EntryChild {
	fn hash<H: Hasher>(&self, state: &mut H) {
		self.0.name.hash(state)
	}
}

/// A VFS entry, representing a directory entry cached in memory.
#[derive(Debug)]
pub struct Entry {
	/// Filename.
	pub name: String,
	/// The parent of the entry.
	///
	/// If `None`, the current entry is the root of the VFS.
	parent: Option<Arc<Entry>>,
	/// The list of cached file entries.
	///
	/// This is not an exhaustive list of the file's entries. Only those that are loaded.
	children: Mutex<HashSet<EntryChild>>,
	/// The node associated with the entry.
	///
	/// If `None`, the file do not actually exist.
	node: Option<Arc<Node>>,
}

impl Entry {
	/// Creates a new entry for the given `node`.
	pub fn from_node(node: Arc<Node>) -> Self {
		Self {
			name: String::new(),
			parent: None,
			children: Default::default(),
			node: Some(node),
		}
	}

	/// If the entry is a mountpoint, return it.
	pub fn get_mountpoint(&self) -> Option<Arc<MountPoint>> {
		let mp_id = self.node.as_ref()?.location.mountpoint_id;
		match &self.parent {
			// The parent is on the same mountpoint: this IS NOT the root of a mountpoint
			Some(parent) if parent.node().location.mountpoint_id == mp_id => None,
			// The parent is on a different mountpoint, or there is no parent: this IS the root of
			// a mountpoint
			Some(_) | None => mountpoint::from_id(mp_id),
		}
	}

	/// Returns a reference to the underlying node.
	///
	/// If the entry represents a non-existent file, the function panics.
	#[inline]
	pub fn node(&self) -> &Arc<Node> {
		self.node
			.as_ref()
			.expect("trying to access a non-existent node")
	}

	/// Helper returning the status of the underlying node.
	///
	/// If the entry represents a non-existent file, the function panics.
	#[inline]
	pub fn stat(&self) -> EResult<Stat> {
		self.node().ops.get_stat(&self.node().location)
	}

	/// Returns the file's type.
	#[inline]
	pub fn get_type(&self) -> EResult<FileType> {
		FileType::from_mode(self.stat()?.mode).ok_or_else(|| errno!(EUCLEAN))
	}

	/// Reads the whole content of the file into a buffer.
	///
	/// **Caution**: the function reads until EOF, meaning the caller should not call this function
	/// on an infinite file.
	pub fn read_all(&self) -> EResult<Vec<u8>> {
		const INCREMENT: usize = 512;
		let len: usize = self
			.node()
			.ops
			.get_stat(&self.node().location)?
			.size
			.try_into()
			.map_err(|_| errno!(EOVERFLOW))?;
		let len = len
			.checked_add(INCREMENT)
			.ok_or_else(|| errno!(EOVERFLOW))?;
		// Add more space to allow check for EOF
		let mut buf = vec![0u8; len]?;
		let mut off = 0;
		// Read until EOF
		loop {
			// If the size has been exceeded, resize the buffer
			if off >= buf.len() {
				let new_size = buf
					.len()
					.checked_add(INCREMENT)
					.ok_or_else(|| errno!(EOVERFLOW))?;
				buf.resize(new_size, 0)?;
			}
			let len =
				self.node()
					.ops
					.read_content(&self.node().location, off as _, &mut buf[off..])?;
			// Reached EOF, stop here
			if len == 0 {
				break;
			}
			off += len;
		}
		// Adjust the size of the buffer
		buf.truncate(off);
		Ok(buf)
	}

	/// Returns the absolute path to reach the entry.
	pub fn get_path(this: &Arc<Self>) -> EResult<PathBuf> {
		if this.parent.is_none() {
			return Ok(PathBuf::root()?);
		}
		let mut buf = vec![0u8; PATH_MAX]?;
		let mut off = PATH_MAX;
		let mut cur = this;
		while let Some(parent) = &cur.parent {
			let len = cur.name.len();
			off = off
				.checked_sub(len + 1)
				.ok_or_else(|| errno!(ENAMETOOLONG))?;
			buf[off] = b'/';
			buf[(off + 1)..(off + len + 1)].copy_from_slice(&cur.name);
			cur = parent;
		}
		buf.rotate_left(off);
		buf.truncate(buf.len() - off);
		Ok(PathBuf::new_unchecked(String::from(buf)))
	}

	/// Releases the entry, removing it the underlying node if no link remain and this was the last
	/// use of it.
	pub fn release(this: Arc<Self>) -> EResult<()> {
		let Some(parent) = &this.parent else {
			// This is the root of the VFS, stop
			return Ok(());
		};
		{
			// Lock to avoid a race condition with `strong_count`
			let mut parent_children = parent.children.lock();
			// If this is **not** the last reference to the current (the one held by its own
			// parent
			// + the one that we hold here)
			if Arc::strong_count(&this) > 2 {
				return Ok(());
			}
			parent_children.remove(&*this.name);
		}
		let Some(c) = Arc::into_inner(this) else {
			// The entry was already detached from its parent before: someone else references it
			return Ok(());
		};
		// Release the inner node if present
		if let Some(node) = c.node {
			Node::release(node)?;
		}
		Ok(())
	}
}

/// The root entry of the VFS.
pub(super) static ROOT: OnceInit<Arc<Entry>> = unsafe { OnceInit::new() };

/// Returns the root entry.
pub fn root() -> Arc<Entry> {
	ROOT.get().clone()
}

/// Settings for a path resolution operation.
#[derive(Clone, Debug)]
pub struct ResolutionSettings {
	/// The location of the root directory for the operation.
	///
	/// Contrary to the `start` field, resolution *cannot* access a parent of this path.
	pub root: Arc<Entry>,
	/// The current working directory, from which the resolution starts.
	///
	/// If `None`, resolution starts from `root`.
	pub cwd: Option<Arc<Entry>>,

	/// The access profile to use for resolution.
	pub access_profile: AccessProfile,

	/// If `true`, the path is resolved for creation, meaning the operation will not fail if the
	/// file does not exist.
	pub create: bool,
	/// If `true` and if the last component of the path is a symbolic link, path resolution
	/// follows it.
	pub follow_link: bool,
}

impl ResolutionSettings {
	/// Kernel access, following symbolic links.
	pub fn kernel_follow() -> Self {
		Self {
			root: root(),
			cwd: None,

			access_profile: AccessProfile::KERNEL,

			create: false,
			follow_link: true,
		}
	}

	/// Kernel access, without following symbolic links.
	pub fn kernel_nofollow() -> Self {
		Self {
			follow_link: false,
			..Self::kernel_follow()
		}
	}

	/// Returns the default for the given process.
	///
	/// `follow_links` tells whether symbolic links are followed.
	pub fn for_process(proc: &Process, follow_links: bool) -> Self {
		let fs = proc.fs.lock();
		Self {
			root: fs.chroot.clone(),
			cwd: Some(fs.cwd.clone()),

			access_profile: fs.access_profile,

			create: false,
			follow_link: follow_links,
		}
	}
}

/// The resolute of the path resolution operation.
#[derive(Debug)]
pub enum Resolved<'s> {
	/// The file has been found.
	Found(Arc<Entry>),
	/// The file can be created.
	///
	/// This variant can be returned only if the `create` field is set to `true` in
	/// [`ResolutionSettings`].
	Creatable {
		/// The parent directory in which the file is to be created.
		parent: Arc<Entry>,
		/// The name of the file to be created.
		name: &'s [u8],
	},
}

/// Resolves an entry with the given `name`, in the given `lookup_dir`.
///
/// If the entry does not exist, the function returns `None`.
fn resolve_entry(lookup_dir: &Arc<Entry>, name: &[u8]) -> EResult<Option<Arc<Entry>>> {
	let mut children = lookup_dir.children.lock();
	// Try to get from cache first
	if let Some(ent) = children.get(name) {
		return if ent.0.node.is_some() {
			Ok(Some(ent.0.clone()))
		} else {
			Ok(None)
		};
	}
	// Not in cache. Try to get from the filesystem
	let Some((entry, ops)) = lookup_dir
		.node()
		.ops
		.entry_by_name(&lookup_dir.node().location, name)?
	else {
		return Ok(None);
	};
	let node = node::insert(Node {
		location: FileLocation {
			// The file is on the same mountpoint as the parent since mountpoint roots are always
			// in cache
			mountpoint_id: lookup_dir.node().location.mountpoint_id,
			inode: entry.inode,
		},
		ops,
	})?;
	// Create entry and insert in parent
	let ent = Arc::new(Entry {
		name: String::try_from(name)?,
		parent: Some(lookup_dir.clone()),
		children: Default::default(),
		node: Some(node),
	})?;
	children.insert(EntryChild(ent.clone()))?;
	Ok(Some(ent))
}

/// Resolves the symbolic link `link` and returns the target.
///
/// Arguments:
/// - `root` is the root directory
/// - `lookup_dir` is the directory from which the resolution of the target starts
/// - `access_profile` is the access profile used for resolution
/// - `symlink_rec` is the number of recursions so far
///
/// Symbolic links are followed recursively, including the last element of the target path.
fn resolve_link(
	link: &Entry,
	root: Arc<Entry>,
	lookup_dir: Arc<Entry>,
	access_profile: AccessProfile,
	symlink_rec: usize,
) -> EResult<Arc<Entry>> {
	// If too many recursions occur, error
	if unlikely(symlink_rec + 1 > SYMLOOP_MAX) {
		return Err(errno!(ELOOP));
	}
	// Read link
	let link_path = PathBuf::try_from(String::from(link.read_all()?))?;
	// Resolve link
	let rs = ResolutionSettings {
		root,
		cwd: Some(lookup_dir),
		access_profile,
		create: false,
		follow_link: true,
	};
	let resolved = resolve_path_impl(&link_path, &rs, symlink_rec + 1)?;
	let Resolved::Found(target) = resolved else {
		// Because `create` is set to `false`
		unreachable!();
	};
	Ok(target)
}

/// Implementation of [`resolve_path`].
///
/// `symlink_rec` is the number of recursions due to symbolic links resolution.
fn resolve_path_impl<'p>(
	path: &'p Path,
	settings: &ResolutionSettings,
	symlink_rec: usize,
) -> EResult<Resolved<'p>> {
	// Get start lookup directory
	let mut lookup_dir = match (path.is_absolute(), &settings.cwd) {
		(false, Some(start)) => start.clone(),
		_ => settings.root.clone(),
	};
	let mut components = path.components();
	let Some(final_component) = components.next_back() else {
		return Ok(Resolved::Found(lookup_dir));
	};
	// Iterate on intermediate components
	for comp in components {
		// Check lookup permission
		let lookup_dir_stat = lookup_dir.stat()?;
		if !settings
			.access_profile
			.can_search_directory(&lookup_dir_stat)
		{
			return Err(errno!(EACCES));
		}
		// Get the name of the next entry
		let name = match comp {
			Component::ParentDir => {
				if let Some(parent) = &lookup_dir.parent {
					lookup_dir = parent.clone();
				}
				continue;
			}
			Component::Normal(name) => name,
			// Ignore
			_ => continue,
		};
		// Get entry
		let entry = resolve_entry(&lookup_dir, name)?.ok_or_else(|| errno!(ENOENT))?;
		match entry.get_type()? {
			FileType::Directory => lookup_dir = entry,
			FileType::Link => {
				lookup_dir = resolve_link(
					&entry,
					settings.root.clone(),
					lookup_dir,
					settings.access_profile,
					symlink_rec,
				)?;
			}
			_ => return Err(errno!(ENOTDIR)),
		}
	}
	// Final component lookup
	let name = match final_component {
		Component::RootDir | Component::CurDir => {
			// If the component is `RootDir`, the entire path equals `/` and `lookup_dir` can only
			// be the root. If the component is `CurDir`, the `lookup_dir` is the target
			return Ok(Resolved::Found(lookup_dir));
		}
		Component::ParentDir => {
			if let Some(parent) = &lookup_dir.parent {
				lookup_dir = parent.clone();
			}
			return Ok(Resolved::Found(lookup_dir));
		}
		Component::Normal(name) => name,
	};
	// Check lookup permission
	let lookup_dir_stat = lookup_dir.stat()?;
	if !settings
		.access_profile
		.can_search_directory(&lookup_dir_stat)
	{
		return Err(errno!(EACCES));
	}
	// Get entry
	let Some(entry) = resolve_entry(&lookup_dir, name)? else {
		// The file does not exist
		return if settings.create {
			Ok(Resolved::Creatable {
				parent: lookup_dir,
				name,
			})
		} else {
			Err(errno!(ENOENT))
		};
	};
	// Resolve symbolic link if necessary
	if settings.follow_link && entry.stat()?.get_type() == Some(FileType::Link) {
		Ok(Resolved::Found(resolve_link(
			&entry,
			settings.root.clone(),
			lookup_dir,
			settings.access_profile,
			symlink_rec,
		)?))
	} else {
		Ok(Resolved::Found(entry))
	}
}

/// Resolves the given `path` with the given `settings`.
///
/// The following conditions can cause errors:
/// - If the path is empty, the function returns [`errno::ENOMEM`].
/// - If a component of the path cannot be accessed with the provided access profile, the function
///   returns [`errno::EACCES`].
/// - If a component of the path (excluding the last) is not a directory nor a symbolic link, the
///   function returns [`errno::ENOTDIR`].
/// - If a component of the path (excluding the last) is a symbolic link and following them is
///   disabled, the function returns [`errno::ENOTDIR`].
/// - If the resolution of the path requires more symbolic link indirections than [`SYMLOOP_MAX`],
///   the function returns [`errno::ELOOP`].
pub fn resolve_path<'p>(path: &'p Path, settings: &ResolutionSettings) -> EResult<Resolved<'p>> {
	// Required by POSIX
	if settings.cwd.is_none() && path.is_empty() {
		return Err(errno!(ENOENT));
	}
	resolve_path_impl(path, settings, 0)
}

/// Like [`get_file_from_path`], but returns `None` is the file does not exist.
pub fn get_file_from_path_opt(
	path: &Path,
	resolution_settings: &ResolutionSettings,
) -> EResult<Option<Arc<Entry>>> {
	let file = match resolve_path(path, resolution_settings)? {
		Resolved::Found(file) => Some(file),
		_ => None,
	};
	Ok(file)
}

/// Returns the file at the given `path`.
///
/// If the file does not exist, the function returns [`errno::ENOENT`].
pub fn get_file_from_path(
	path: &Path,
	resolution_settings: &ResolutionSettings,
) -> EResult<Arc<Entry>> {
	get_file_from_path_opt(path, resolution_settings)?.ok_or_else(|| errno!(ENOENT))
}

/// Creates a file, adds it to the VFS, then returns it.
///
/// Arguments:
/// - `parent` is the parent directory of the file to be created
/// - `name` is the name of the file to be created
/// - `ap` is access profile to check permissions. This also determines the UID and GID to be used
///   for the created file
/// - `stat` is the status of the newly created file
///
/// From the provided `stat`, the following fields are ignored:
/// - `nlink`
/// - `uid`
/// - `gid`
///
/// `uid` and `gid` are set according to `ap`.
///
/// The following errors can be returned:
/// - The filesystem is read-only: [`errno::EROFS`]
/// - I/O failed: [`errno::EIO`]
/// - Permissions to create the file are not fulfilled for the given `ap`: [`errno::EACCES`]
/// - `parent` is not a directory: [`errno::ENOTDIR`]
/// - The file already exists: [`errno::EEXIST`]
///
/// Other errors can be returned depending on the underlying filesystem.
pub fn create_file(
	parent: Arc<Entry>,
	name: &[u8],
	ap: &AccessProfile,
	mut stat: Stat,
) -> EResult<Arc<Entry>> {
	let parent_stat = parent.stat()?;
	// Validation
	if parent_stat.get_type() != Some(FileType::Directory) {
		return Err(errno!(ENOTDIR));
	}
	if !ap.can_write_directory(&parent_stat) {
		return Err(errno!(EACCES));
	}
	stat.uid = ap.euid;
	let gid = if parent_stat.mode & perm::S_ISGID != 0 {
		// If SGID is set, the newly created file shall inherit the group ID of the
		// parent directory
		parent_stat.gid
	} else {
		ap.egid
	};
	stat.gid = gid;
	// Add file to filesystem
	let (inode, ops) = parent
		.node()
		.ops
		.add_file(&parent.node().location, name, stat)?;
	let location = FileLocation {
		mountpoint_id: parent.node().location.mountpoint_id,
		inode,
	};
	let node = node::get_or_insert(location, ops)?;
	// Create entry and insert it in parent
	let entry = Arc::new(Entry {
		name: String::try_from(name)?,
		parent: Some(parent.clone()),
		children: Default::default(),
		node: Some(node),
	})?;
	parent.children.lock().insert(EntryChild(entry.clone()))?;
	Ok(entry)
}

/// Creates a new hard link to the given target file.
///
/// Arguments:
/// - `parent` is the parent directory where the new link will be created
/// - `name` is the name of the link
/// - `target` is the target file
/// - `ap` is the access profile to check permissions
///
/// The following errors can be returned:
/// - The filesystem is read-only: [`errno::EROFS`]
/// - I/O failed: [`errno::EIO`]
/// - Permissions to create the link are not fulfilled for the given `ap`: [`errno::EACCES`]
/// - The number of links to the file is larger than [`LINK_MAX`]: [`errno::EMLINK`]
/// - `target` is a directory: [`errno::EPERM`]
///
/// Other errors can be returned depending on the underlying filesystem.
pub fn link(parent: &Entry, name: &[u8], target: &Entry, ap: &AccessProfile) -> EResult<()> {
	let parent_stat = parent.stat()?;
	// Validation
	if parent_stat.get_type() != Some(FileType::Directory) {
		return Err(errno!(ENOTDIR));
	}
	let target_stat = target.stat()?;
	if target_stat.get_type() == Some(FileType::Directory) {
		return Err(errno!(EPERM));
	}
	if target_stat.nlink >= LINK_MAX as u16 {
		return Err(errno!(EMLINK));
	}
	if !ap.can_write_directory(&parent_stat) {
		return Err(errno!(EACCES));
	}
	// Check the target and source are both on the same mountpoint
	if parent.node().location.mountpoint_id != target.node().location.mountpoint_id {
		return Err(errno!(EXDEV));
	}
	parent
		.node()
		.ops
		.link(&parent.node().location, name, target.node().location.inode)?;
	Ok(())
}

/// Removes a hard link to a file.
///
/// Arguments:
/// - `parent` is the parent directory of the file to remove
/// - `name` is the name of the file to remove
/// - `ap` is the access profile to check permissions
///
/// The following errors can be returned:
/// - The filesystem is read-only: [`errno::EROFS`]
/// - I/O failed: [`errno::EIO`]
/// - The link does not exist: [`errno::ENOENT`]
/// - Permissions to remove the link are not fulfilled for the given `ap`: [`errno::EACCES`]
/// - The file to remove is a mountpoint: [`errno::EBUSY`]
///
/// Other errors can be returned depending on the underlying filesystem.
pub fn unlink(parent: Arc<Entry>, name: &[u8], ap: &AccessProfile) -> EResult<()> {
	let parent_stat = parent.stat()?;
	// Check permission
	if parent_stat.get_type() != Some(FileType::Directory) {
		return Err(errno!(ENOTDIR));
	}
	if !ap.can_write_directory(&parent_stat) {
		return Err(errno!(EACCES));
	}
	// Lock now to avoid race conditions
	let mut children = parent.children.lock();
	match children.get(name) {
		// The entry is in cache
		Some(EntryChild(entry)) => {
			// If the file to remove is a mountpoint, error
			if parent.node().location.mountpoint_id != entry.node().location.mountpoint_id {
				return Err(errno!(EBUSY));
			}
			let stat = entry.stat()?;
			// Check permission
			let has_sticky_bit = parent_stat.mode & S_ISVTX != 0;
			if has_sticky_bit && ap.euid != stat.uid && ap.euid != parent_stat.uid {
				return Err(errno!(EACCES));
			}
			// Remove link from filesystem
			parent.node().ops.unlink(&parent.node().location, name)?;
			// Remove link from cache
			let EntryChild(ent) = children.remove(name).unwrap();
			drop(children);
			Entry::release(ent)
		}
		// The entry is not in cache
		None => {
			let (entry, ops) = parent
				.node()
				.ops
				.entry_by_name(&parent.node().location, name)?
				.ok_or_else(|| errno!(ENOENT))?;
			let loc = FileLocation {
				// The entry cannot be a mountpoint since it is not in cache
				mountpoint_id: parent.node().location.mountpoint_id,
				inode: entry.inode,
			};
			let stat = ops.get_stat(&loc)?;
			// Check permission
			let has_sticky_bit = parent_stat.mode & S_ISVTX != 0;
			if has_sticky_bit && ap.euid != stat.uid && ap.euid != parent_stat.uid {
				return Err(errno!(EACCES));
			}
			// Remove link from filesystem
			parent.node().ops.unlink(&parent.node().location, name)?;
			node::try_remove(&loc, &*ops)
		}
	}
}

/// Helper function to remove a hard link from a given `path`.
pub fn unlink_from_path(path: &Path, resolution_settings: &ResolutionSettings) -> EResult<()> {
	let file_name = path.file_name().ok_or_else(|| errno!(ENOENT))?;
	let parent = path.parent().ok_or_else(|| errno!(ENOENT))?;
	let parent = get_file_from_path(parent, resolution_settings)?;
	unlink(parent, file_name, &resolution_settings.access_profile)
}

/// Implementation of [`super::FileOps`] for file from the VFS.
#[derive(Debug)]
pub struct FileOps;

impl super::FileOps for FileOps {
	fn get_stat(&self, file: &File) -> EResult<Stat> {
		file.vfs_entry.as_ref().unwrap().stat()
	}

	fn acquire(&self, _file: &File) {}

	fn release(&self, _file: &File) {}

	fn poll(&self, file: &File, mask: u32) -> EResult<u32> {
		let stat = self.get_stat(file)?;
		let dev_type = stat.get_type().and_then(FileType::to_device_type);
		match dev_type {
			Some(dev_type) => device::get(&DeviceID {
				dev_type,
				major: stat.dev_major,
				minor: stat.dev_minor,
			})
			.ok_or_else(|| errno!(ENODEV))?
			.get_io()
			.poll(mask),
			None => todo!(),
		}
	}

	fn ioctl(&self, file: &File, request: Request, argp: *const c_void) -> EResult<u32> {
		let stat = self.get_stat(file)?;
		let dev_type = stat
			.get_type()
			.and_then(FileType::to_device_type)
			.ok_or_else(|| errno!(ENOTTY))?;
		device::get(&DeviceID {
			dev_type,
			major: stat.dev_major,
			minor: stat.dev_minor,
		})
		.ok_or_else(|| errno!(ENODEV))?
		.get_io()
		.ioctl(request, argp)
	}

	fn read(&self, file: &File, off: u64, buf: &mut [u8]) -> EResult<usize> {
		if unlikely(!file.can_read()) {
			return Err(errno!(EACCES));
		}
		let stat = self.get_stat(file)?;
		let dev_type = stat.get_type().and_then(FileType::to_device_type);
		match dev_type {
			Some(dev_type) => device::get(&DeviceID {
				dev_type,
				major: stat.dev_major,
				minor: stat.dev_minor,
			})
			.ok_or_else(|| errno!(ENODEV))?
			.get_io()
			.read_bytes(off, buf),
			None => {
				let node = file.vfs_entry.as_ref().unwrap().node();
				node.ops.read_content(&node.location, off, buf)
			}
		}
	}

	fn write(&self, file: &File, off: u64, buf: &[u8]) -> EResult<usize> {
		if unlikely(!file.can_write()) {
			return Err(errno!(EACCES));
		}
		let stat = self.get_stat(file)?;
		let dev_type = stat.get_type().and_then(FileType::to_device_type);
		match dev_type {
			Some(dev_type) => device::get(&DeviceID {
				dev_type,
				major: stat.dev_major,
				minor: stat.dev_minor,
			})
			.ok_or_else(|| errno!(ENODEV))?
			.get_io()
			.write_bytes(off, buf),
			None => {
				let node = file.vfs_entry.as_ref().unwrap().node();
				node.ops.write_content(&node.location, off, buf)
			}
		}
	}
}
