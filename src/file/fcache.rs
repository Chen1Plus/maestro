//! The files cache stores files in memory to avoid accessing the disk each times.

use crate::errno::Errno;
use crate::errno;
use crate::file::File;
use crate::file::FileContent;
use crate::file::FileType;
use crate::file::Gid;
use crate::file::Mode;
use crate::file::Uid;
use crate::file::mountpoint;
use crate::file::path::Path;
use crate::limits;
use crate::util::FailableClone;
use crate::util::container::hashmap::HashMap;
use crate::util::container::string::String;
use crate::util::container::vec::Vec;
use crate::util::lock::Mutex;
use crate::util::ptr::SharedPtr;

/// The size of the files pool.
const FILES_POOL_SIZE: usize = 1024;

// TODO If a filesystem doesn't return entries `.` and `..`, add them

/// Cache storing files in memory. This cache allows to speedup accesses to the disk. It is
/// synchronized with the disk when necessary.
pub struct FCache {
	/// The pool of cached files.
	pool: [Option<SharedPtr<File>>; FILES_POOL_SIZE],
	/// The list of free slots in the pool.
	pool_free: Vec<usize>,

	/// Collection mapping file paths to their slot index.
	pool_paths: HashMap<Path, usize>,
	/// Collection mapping a number of accesses to a slot index.
	access_count: Vec<(usize, usize)>,
}

impl FCache {
	/// Creates a new instance.
	pub fn new() -> Result<Self, Errno> {
		const INIT: Option<SharedPtr<File>> = None;

		Ok(Self {
			pool: [INIT; FILES_POOL_SIZE],
			pool_free: Vec::new(),

			pool_paths: HashMap::new(),
			access_count: Vec::new(),
		})
	}

	/// Loads the file with the given path `path`. If the file is already loaded, the behaviour is
	/// undefined.
	fn load_file(&mut self, _path: &Path) {
		/*let len = self.pool.len();
		if len >= FILES_POOL_SIZE {
			self.files_pool.pop();
			self.accesses_pool.pop();
		}*/

		// TODO Push file
	}

	/// Synchonizes the cache to the disks, then empties it.
	pub fn flush_all(&mut self) -> Result<(), Errno> {
		// TODO
		todo!();
	}

	// TODO Use the cache
	/// Returns a reference to the file at path `path`. If the file doesn't exist, the function
	/// returns None.
	/// If the path is relative, the function starts from the root.
	/// If the file isn't present in the pool, the function shall load it.
	/// `uid` is the User ID of the user creating the file.
	/// `gid` is the Group ID of the user creating the file.
	/// `follow_links` is true, the function follows symbolic links.
	/// `follows_count` is the number of links that have been followed since the beginning of the
	/// path resolution.
	fn get_file_from_path_(&mut self, path: &Path, uid: Uid, gid: Gid, follow_links: bool,
		follows_count: usize) -> Result<SharedPtr<File>, Errno> {
		let path = Path::root().concat(path)?;

		// Getting the path's deepest mountpoint
		let mountpoint_mutex = mountpoint::get_deepest(&path).ok_or_else(|| errno!(ENOENT))?;
		let mountpoint_guard = mountpoint_mutex.lock();
		let mountpoint = mountpoint_guard.get_mut();

		// Getting the IO interface
		let io_mutex = mountpoint.get_source().get_io()?;
		let io_guard = io_mutex.lock();
		let io = io_guard.get_mut();

		// Getting the path from the start of the filesystem to the file
		let inner_path = path.range_from(mountpoint.get_path().get_elements_count()..)?;

		// The filesystem
		let fs = mountpoint.get_filesystem();

		// The root inode
		let mut inode = fs.get_root_inode(io)?;
		let mut file = fs.load_file(io, inode, String::new())?;
		// If the path is empty, return the root
		if inner_path.is_empty() {
			return SharedPtr::new(file);
		}
		// Checking permissions
		if !file.can_read(uid, gid) {
			return Err(errno!(EPERM));
		}

		for i in 0..inner_path.get_elements_count() {
			inode = fs.get_inode(io, Some(inode), &inner_path[i])?;

			// Checking permissions
			file = fs.load_file(io, inode, inner_path[i].failable_clone()?)?;
			if i < inner_path.get_elements_count() - 1 && !file.can_read(uid, gid) {
				return Err(errno!(EPERM));
			}

			if follow_links {
				// If symbolic link, resolve it
				if let FileContent::Link(link_path) = file.get_file_content() {
					if follows_count > limits::SYMLOOP_MAX {
						return Err(errno!(ELOOP));
					}

					let mut parent_path = path.failable_clone()?;
					parent_path.pop();

					let link_path = Path::from_str(link_path.as_bytes(), false)?;
					let new_path = parent_path.concat(&link_path)?;

					drop(io_guard);
					drop(mountpoint_guard);
					return self.get_file_from_path_(&new_path, uid, gid, follow_links,
						follows_count + 1);
				}
			}
		}

		let mut parent_path = path.failable_clone()?;
		parent_path.pop();
		file.set_parent_path(parent_path);

		SharedPtr::new(file)
	}

	// TODO Add a param to choose between the mountpoint and the fs root?
	/// Returns a reference to the file at path `path`. If the file doesn't exist, the function
	/// returns an error.
	/// If the path is relative, the function starts from the root.
	/// If the file isn't present in the pool, the function shall load it.
	/// `uid` is the User ID of the user creating the file.
	/// `gid` is the Group ID of the user creating the file.
	/// `follow_links` is true, the function follows symbolic links.
	pub fn get_file_from_path(&mut self, path: &Path, uid: Uid, gid: Gid, follow_links: bool)
		-> Result<SharedPtr<File>, Errno> {
		self.get_file_from_path_(path, uid, gid, follow_links, 0)
	}

	// TODO Use the cache
	/// Returns a reference to the file `name` located in the directory `parent`. If the file
	/// doesn't exist, the function returns an error.
	/// `parent` is the parent directory.
	/// `name` is the name of the file.
	/// `uid` is the User ID of the user creating the file.
	/// `gid` is the Group ID of the user creating the file.
	/// `follow_links` is true, the function follows symbolic links.
	pub fn get_file_from_parent(&mut self, parent: &mut File, name: String, uid: Uid, gid: Gid,
		follow_links: bool) -> Result<SharedPtr<File>, Errno> {
		// Checking for errors
		if parent.get_file_type() != FileType::Directory {
			return Err(errno!(ENOTDIR));
		}
		if !parent.can_read(uid, gid) {
			return Err(errno!(EPERM));
		}

		// Getting the path's deepest mountpoint
		let mountpoint_mutex = parent.get_location().get_mountpoint()
			.ok_or_else(|| errno!(ENOENT))?;
		let mountpoint_guard = mountpoint_mutex.lock();
		let mountpoint = mountpoint_guard.get_mut();

		// Getting the IO interface
		let io_mutex = mountpoint.get_source().get_io()?;
		let io_guard = io_mutex.lock();
		let io = io_guard.get_mut();

		// The filesystem
		let fs = mountpoint.get_filesystem();

		let inode = fs.get_inode(io, Some(parent.get_location().get_inode()), &name)?;
		let mut file = fs.load_file(io, inode, name)?;

		if follow_links {
			if let FileContent::Link(link_path) = file.get_file_content() {
				let link_path = Path::from_str(link_path.as_bytes(), false)?;
				let new_path = parent.get_path()?.concat(&link_path)?;

				drop(io_guard);
				drop(mountpoint_guard);
				return self.get_file_from_path_(&new_path, uid, gid, follow_links, 1);
			}
		}

		file.set_parent_path(parent.get_path()?);
		SharedPtr::new(file)
	}

	// TODO Use the cache
	/// Creates a file, adds it to the VFS, then returns it. The file will be located into the
	/// directory `parent`.
	/// If `parent` is not a directory, the function returns an error.
	/// `name` is the name of the file.
	/// `uid` is the id of the owner user.
	/// `gid` is the id of the owner group.
	/// `mode` is the permission of the file.
	/// `content` is the content of the file. This value also determines the file type.
	pub fn create_file(&mut self, parent: &mut File, name: String, uid: Uid, gid: Gid, mode: Mode,
		content: FileContent) -> Result<SharedPtr<File>, Errno> {
		// Checking for errors
		if parent.get_file_type() != FileType::Directory {
			return Err(errno!(ENOTDIR));
		}
		if !parent.can_write(uid, gid) {
			return Err(errno!(EPERM));
		}

		// Getting the mountpoint
		let mountpoint_mutex = parent.get_location().get_mountpoint()
			.ok_or_else(|| errno!(ENOENT))?;
		let mountpoint_guard = mountpoint_mutex.lock();
		let mountpoint = mountpoint_guard.get_mut();

		// Getting the IO interface
		let io_mutex = mountpoint.get_source().get_io()?;
		let io_guard = io_mutex.lock();
		let io = io_guard.get_mut();

		let fs = mountpoint.get_filesystem();
		if fs.is_readonly() {
			return Err(errno!(EROFS));
		}

		// The parent directory's inode
		let parent_inode = parent.get_location().get_inode();
		// Adding the file to the filesystem
		let mut file = fs.add_file(io, parent_inode, name, uid, gid, mode, content)?;

		// Adding the file to the parent's entries
		file.set_parent_path(parent.get_path()?);
		parent.add_entry(file.get_name().failable_clone()?, file.to_dir_entry())?;

		SharedPtr::new(file)
	}

	// TODO Use the cache
	/// Removes the file `file` from the VFS.
	/// If the file doesn't exist, the function returns an error.
	/// If the file is a non-empty directory, the function returns an error.
	/// `uid` is the User ID of the user removing the file.
	/// `gid` is the Group ID of the user removing the file.
	pub fn remove_file(&mut self, file: &File, uid: Uid, gid: Gid) -> Result<(), Errno> {
		if file.is_busy() {
			return Err(errno!(EBUSY));
		}

		// The parent directory.
		let parent_mutex = self.get_file_from_path(file.get_parent_path(), uid, gid, true)?;
		let parent_guard = parent_mutex.lock();
		let parent = parent_guard.get();
		let parent_inode = parent.get_location().get_inode();

		// Checking permissions
		if !file.can_write(uid, gid) || !parent.can_write(uid, gid) {
			return Err(errno!(EPERM));
		}

		// Getting the mountpoint
		let mountpoint_mutex = file.get_location()
			.get_mountpoint()
			.ok_or_else(|| errno!(ENOENT))?;
		let mountpoint_guard = mountpoint_mutex.lock();
		let mountpoint = mountpoint_guard.get_mut();

		// Getting the IO interface
		let io_mutex = mountpoint.get_source().get_io()?;
		let io_guard = io_mutex.lock();
		let io = io_guard.get_mut();

		// Removing the file
		let fs = mountpoint.get_filesystem();
		fs.remove_file(io, parent_inode, file.get_name())?;

		Ok(())
	}
}

/// The instance of the file cache.
static FILES_CACHE: Mutex<Option<FCache>> = Mutex::new(None);

/// Returns a mutable reference to the file cache.
/// If the cache is not initialized, the Option is None.
pub fn get() -> &'static Mutex<Option<FCache>> {
	&FILES_CACHE
}
