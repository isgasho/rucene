// Copyright 2019 Zhizhesihai (Beijing) Technology Limited.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// See the License for the specific language governing permissions and
// limitations under the License.

use std::collections::HashSet;
use std::fmt;
use std::path::PathBuf;
use std::sync::Arc;

use core::store::{BufferedChecksumIndexInput, DataOutput, IndexInput, IndexOutput};
use core::store::{FlushInfo, Lock, MergeInfo};
use error::Result;

#[derive(PartialEq, Eq, Clone, Copy)]
pub enum IOContext {
    Read(bool),
    Default,
    Flush(FlushInfo),
    Merge(MergeInfo),
}

impl IOContext {
    pub const READ: IOContext = IOContext::Read(false);
    pub const READ_ONCE: IOContext = IOContext::Read(true);
    pub fn is_merge(&self) -> bool {
        match self {
            IOContext::Merge(_) => true,
            _ => false,
        }
    }
}

pub trait Directory: fmt::Display {
    type LK: Lock;
    type IndexOutput: IndexOutput;
    type TempOutput: IndexOutput;
    /// Returns an array of strings, one for each entry in the directory, in sorted (UTF16,
    /// java's String.compare) order.
    fn list_all(&self) -> Result<Vec<String>>;

    /// Returns the length of a file in the directory. This method follows the
    /// following contract:
    fn file_length(&self, name: &str) -> Result<i64>;

    /// Creates a new, empty file in the directory with the given name.
    /// Returns a stream writing this file.
    fn create_output(&self, name: &str, context: &IOContext) -> Result<Self::IndexOutput>;

    fn open_input(&self, name: &str, ctx: &IOContext) -> Result<Box<dyn IndexInput>>;

    fn open_checksum_input(
        &self,
        name: &str,
        ctx: &IOContext,
    ) -> Result<BufferedChecksumIndexInput> {
        let input = self.open_input(name, ctx)?;
        Ok(BufferedChecksumIndexInput::new(input))
    }

    /// Returns an obtained {@link Lock}.
    /// @param name the name of the lock file
    /// @throws LockObtainFailedException (optional specific exception) if the lock could
    ///         not be obtained because it is currently held elsewhere.
    /// @throws IOException if any i/o error occurs attempting to gain the lock
    fn obtain_lock(&self, name: &str) -> Result<Self::LK>;

    fn create_temp_output(
        &self,
        prefix: &str,
        suffix: &str,
        ctx: &IOContext,
    ) -> Result<Self::TempOutput>;

    fn delete_file(&self, name: &str) -> Result<()>;

    /// Ensure that any writes to these files are moved to
    /// stable storage.  Lucene uses this to properly commit
    /// changes to the index, to prevent a machine/OS crash
    /// from corrupting the index.
    ///
    /// NOTE: Clients may call this method for same files over
    /// and over again, so some impls might optimize for that.
    /// For other impls the operation can be a noop, for various
    /// reasons.
    fn sync(&self, name: &HashSet<String>) -> Result<()>;

    /// Ensure that directory metadata, such as recent file renames, are made durable.
    fn sync_meta_data(&self) -> Result<()>;

    fn rename(&self, source: &str, dest: &str) -> Result<()>;

    fn copy_from<D: Directory>(
        &self,
        from: Arc<D>,
        src: &str,
        dest: &str,
        ctx: &IOContext,
    ) -> Result<()> {
        let mut _success = false;
        let mut is = from.open_input(src, ctx)?;
        let mut os = self.create_output(dest, ctx)?;

        let length = is.len();
        os.copy_bytes(is.as_mut(), length as usize)?;

        _success = true;

        //        if (!_success) {
        //            IOUtils.deleteFilesIgnoringExceptions(this, dest);
        //        }

        Ok(())
    }

    fn create_files(&self) -> HashSet<String> {
        unreachable!()
    }

    fn resolve(&self, _name: &str) -> PathBuf {
        unimplemented!()
    }
}

/// This struct makes a best-effort check that a provided
/// `Lock` is valid before any destructive filesystem operation.
pub struct LockValidatingDirectoryWrapper<D: Directory> {
    dir: Arc<D>,
    write_lock: Arc<D::LK>,
}

impl<D: Directory> LockValidatingDirectoryWrapper<D> {
    pub fn new(dir: Arc<D>, write_lock: Arc<D::LK>) -> Self {
        LockValidatingDirectoryWrapper { dir, write_lock }
    }
}

impl<D: Directory> Directory for LockValidatingDirectoryWrapper<D> {
    type LK = D::LK;
    type IndexOutput = D::IndexOutput;
    type TempOutput = D::TempOutput;

    fn list_all(&self) -> Result<Vec<String>> {
        self.dir.list_all()
    }

    fn file_length(&self, name: &str) -> Result<i64> {
        self.dir.file_length(name)
    }

    fn create_output(&self, name: &str, context: &IOContext) -> Result<Self::IndexOutput> {
        self.write_lock.ensure_valid()?;
        self.dir.create_output(name, context)
    }

    fn open_input(&self, name: &str, ctx: &IOContext) -> Result<Box<dyn IndexInput>> {
        self.dir.open_input(name, ctx)
    }

    fn obtain_lock(&self, name: &str) -> Result<Self::LK> {
        self.dir.obtain_lock(name)
    }

    fn create_temp_output(
        &self,
        prefix: &str,
        suffix: &str,
        ctx: &IOContext,
    ) -> Result<Self::TempOutput> {
        self.dir.create_temp_output(prefix, suffix, ctx)
    }

    fn delete_file(&self, name: &str) -> Result<()> {
        self.write_lock.ensure_valid()?;
        self.dir.delete_file(name)
    }

    fn sync(&self, name: &HashSet<String>) -> Result<()> {
        self.write_lock.ensure_valid()?;
        self.dir.sync(name)
    }

    fn sync_meta_data(&self) -> Result<()> {
        self.write_lock.ensure_valid()?;
        self.dir.sync_meta_data()
    }

    fn rename(&self, source: &str, dest: &str) -> Result<()> {
        self.write_lock.ensure_valid()?;
        self.dir.rename(source, dest)
    }

    fn copy_from<D1: Directory>(
        &self,
        from: Arc<D1>,
        src: &str,
        dest: &str,
        ctx: &IOContext,
    ) -> Result<()> {
        self.write_lock.ensure_valid()?;
        self.dir.copy_from(from, src, dest, ctx)
    }
}

impl<D: Directory> fmt::Display for LockValidatingDirectoryWrapper<D> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "LockValidatingDirectoryWrapper({})", self.dir.as_ref())
    }
}
