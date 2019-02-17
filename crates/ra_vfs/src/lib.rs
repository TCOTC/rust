//! VFS stands for Virtual File System.
//!
//! When doing analysis, we don't want to do any IO, we want to keep all source
//! code in memory. However, the actual source code is stored on disk, so you
//! need to get it into the memory in the first place somehow. VFS is the
//! component which does this.
//!
//! It is also responsible for watching the disk for changes, and for merging
//! editor state (modified, unsaved files) with disk state.
//!
//! TODO: Some LSP clients support watching the disk, so this crate should to
//! support custom watcher events (related to
//! <https://github.com/rust-analyzer/rust-analyzer/issues/131>)
//!
//! VFS is based on a concept of roots: a set of directories on the file system
//! which are watched for changes. Typically, there will be a root for each
//! Cargo package.
mod roots;
mod io;

use std::{
    fmt, fs, mem,
    path::{Path, PathBuf},
    sync::Arc,
};

use crossbeam_channel::Receiver;
use ra_arena::{impl_arena_id, Arena, RawId, map::ArenaMap};
use relative_path::{RelativePath, RelativePathBuf};
use rustc_hash::{FxHashMap, FxHashSet};

use crate::{
    io::{TaskResult, Worker},
    roots::Roots,
};

pub use crate::{
    io::TaskResult as VfsTask,
    roots::VfsRoot,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct VfsFile(pub RawId);
impl_arena_id!(VfsFile);

struct VfsFileData {
    root: VfsRoot,
    path: RelativePathBuf,
    is_overlayed: bool,
    text: Arc<String>,
}

pub struct Vfs {
    roots: Arc<Roots>,
    files: Arena<VfsFile, VfsFileData>,
    root2files: ArenaMap<VfsRoot, FxHashSet<VfsFile>>,
    pending_changes: Vec<VfsChange>,
    worker: Worker,
}

impl fmt::Debug for Vfs {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("Vfs")
            .field("n_roots", &self.roots.len())
            .field("n_files", &self.files.len())
            .field("n_pending_changes", &self.pending_changes.len())
            .finish()
    }
}

impl Vfs {
    pub fn new(roots: Vec<PathBuf>) -> (Vfs, Vec<VfsRoot>) {
        let roots = Arc::new(Roots::new(roots));
        let worker = io::start(Arc::clone(&roots));
        let mut root2files = ArenaMap::default();

        for root in roots.iter() {
            root2files.insert(root, Default::default());
            worker.sender().send(io::Task::AddRoot { root }).unwrap();
        }
        let res =
            Vfs { roots, files: Arena::default(), root2files, worker, pending_changes: Vec::new() };
        let vfs_roots = res.roots.iter().collect();
        (res, vfs_roots)
    }

    pub fn root2path(&self, root: VfsRoot) -> PathBuf {
        self.roots.path(root).to_path_buf()
    }

    pub fn path2file(&self, path: &Path) -> Option<VfsFile> {
        if let Some((_root, _path, Some(file))) = self.find_root(path) {
            return Some(file);
        }
        None
    }

    pub fn file2path(&self, file: VfsFile) -> PathBuf {
        let rel_path = &self.files[file].path;
        let root_path = &self.roots.path(self.files[file].root);
        rel_path.to_path(root_path)
    }

    pub fn n_roots(&self) -> usize {
        self.roots.len()
    }

    pub fn load(&mut self, path: &Path) -> Option<VfsFile> {
        if let Some((root, rel_path, file)) = self.find_root(path) {
            return if let Some(file) = file {
                Some(file)
            } else {
                let text = fs::read_to_string(path).unwrap_or_default();
                let text = Arc::new(text);
                let file = self.add_file(root, rel_path.clone(), Arc::clone(&text), false);
                let change = VfsChange::AddFile { file, text, root, path: rel_path };
                self.pending_changes.push(change);
                Some(file)
            };
        }
        None
    }

    pub fn add_file_overlay(&mut self, path: &Path, text: String) -> Option<VfsFile> {
        let (root, rel_path, file) = self.find_root(path)?;
        if let Some(file) = file {
            self.do_change_file(file, text, true);
            Some(file)
        } else {
            self.do_add_file(root, rel_path, text, true)
        }
    }

    pub fn change_file_overlay(&mut self, path: &Path, new_text: String) {
        if let Some((_root, _path, file)) = self.find_root(path) {
            let file = file.expect("can't change a file which wasn't added");
            self.do_change_file(file, new_text, true);
        }
    }

    pub fn remove_file_overlay(&mut self, path: &Path) -> Option<VfsFile> {
        let (root, rel_path, file) = self.find_root(path)?;
        let file = file.expect("can't remove a file which wasn't added");
        let full_path = rel_path.to_path(&self.roots.path(root));
        if let Ok(text) = fs::read_to_string(&full_path) {
            self.do_change_file(file, text, true);
        } else {
            self.do_remove_file(root, rel_path, file, true);
        }
        Some(file)
    }

    pub fn commit_changes(&mut self) -> Vec<VfsChange> {
        mem::replace(&mut self.pending_changes, Vec::new())
    }

    pub fn task_receiver(&self) -> &Receiver<io::TaskResult> {
        self.worker.receiver()
    }

    pub fn handle_task(&mut self, task: io::TaskResult) {
        match task {
            TaskResult::BulkLoadRoot { root, files } => {
                let mut cur_files = Vec::new();
                // While we were scanning the root in the background, a file might have
                // been open in the editor, so we need to account for that.
                let existing = self.root2files[root]
                    .iter()
                    .map(|&file| (self.files[file].path.clone(), file))
                    .collect::<FxHashMap<_, _>>();
                for (path, text) in files {
                    if let Some(&file) = existing.get(&path) {
                        let text = Arc::clone(&self.files[file].text);
                        cur_files.push((file, path, text));
                        continue;
                    }
                    let text = Arc::new(text);
                    let file = self.add_file(root, path.clone(), Arc::clone(&text), false);
                    cur_files.push((file, path, text));
                }

                let change = VfsChange::AddRoot { root, files: cur_files };
                self.pending_changes.push(change);
            }
            TaskResult::SingleFile { root, path, text } => {
                match (self.find_file(root, &path), text) {
                    (Some(file), None) => {
                        self.do_remove_file(root, path, file, false);
                    }
                    (None, Some(text)) => {
                        self.do_add_file(root, path, text, false);
                    }
                    (Some(file), Some(text)) => {
                        self.do_change_file(file, text, false);
                    }
                    (None, None) => (),
                }
            }
        }
    }

    fn do_add_file(
        &mut self,
        root: VfsRoot,
        path: RelativePathBuf,
        text: String,
        is_overlay: bool,
    ) -> Option<VfsFile> {
        let text = Arc::new(text);
        let file = self.add_file(root, path.clone(), text.clone(), is_overlay);
        self.pending_changes.push(VfsChange::AddFile { file, root, path, text });
        Some(file)
    }

    fn do_change_file(&mut self, file: VfsFile, text: String, is_overlay: bool) {
        if !is_overlay && self.files[file].is_overlayed {
            return;
        }
        let text = Arc::new(text);
        self.change_file(file, text.clone(), is_overlay);
        self.pending_changes.push(VfsChange::ChangeFile { file, text });
    }

    fn do_remove_file(
        &mut self,
        root: VfsRoot,
        path: RelativePathBuf,
        file: VfsFile,
        is_overlay: bool,
    ) {
        if !is_overlay && self.files[file].is_overlayed {
            return;
        }
        self.remove_file(file);
        self.pending_changes.push(VfsChange::RemoveFile { root, path, file });
    }

    fn add_file(
        &mut self,
        root: VfsRoot,
        path: RelativePathBuf,
        text: Arc<String>,
        is_overlayed: bool,
    ) -> VfsFile {
        let data = VfsFileData { root, path, text, is_overlayed };
        let file = self.files.alloc(data);
        self.root2files.get_mut(root).unwrap().insert(file);
        file
    }

    fn change_file(&mut self, file: VfsFile, new_text: Arc<String>, is_overlayed: bool) {
        let mut file_data = &mut self.files[file];
        file_data.text = new_text;
        file_data.is_overlayed = is_overlayed;
    }

    fn remove_file(&mut self, file: VfsFile) {
        // FIXME: use arena with removal
        self.files[file].text = Default::default();
        self.files[file].path = Default::default();
        let root = self.files[file].root;
        let removed = self.root2files.get_mut(root).unwrap().remove(&file);
        assert!(removed);
    }

    fn find_root(&self, path: &Path) -> Option<(VfsRoot, RelativePathBuf, Option<VfsFile>)> {
        let (root, path) = self.roots.find(&path)?;
        let file = self.find_file(root, &path);
        Some((root, path, file))
    }

    fn find_file(&self, root: VfsRoot, path: &RelativePath) -> Option<VfsFile> {
        self.root2files[root].iter().map(|&it| it).find(|&file| self.files[file].path == path)
    }
}

#[derive(Debug, Clone)]
pub enum VfsChange {
    AddRoot { root: VfsRoot, files: Vec<(VfsFile, RelativePathBuf, Arc<String>)> },
    AddFile { root: VfsRoot, file: VfsFile, path: RelativePathBuf, text: Arc<String> },
    RemoveFile { root: VfsRoot, file: VfsFile, path: RelativePathBuf },
    ChangeFile { file: VfsFile, text: Arc<String> },
}
