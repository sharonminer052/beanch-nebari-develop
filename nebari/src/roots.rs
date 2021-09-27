use std::{
    any::Any,
    borrow::Cow,
    collections::HashMap,
    convert::Infallible,
    fmt::{Debug, Display},
    fs,
    marker::PhantomData,
    ops::RangeBounds,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicU16, Ordering},
        Arc,
    },
};

use flume::Sender;
use once_cell::sync::Lazy;
use parking_lot::Mutex;

use crate::{
    context::Context,
    transaction::{TransactionHandle, TransactionManager},
    tree::{
        self, state::AnyTreeState, CompareSwap, KeyEvaluation, KeyOperation, Modification,
        Operation, State, TreeFile, TreeRoot, VersionedTreeRoot,
    },
    Buffer, ChunkCache, Error, FileManager, ManagedFile, Vault,
};

/// A multi-tree transactional B-Tree database.
#[derive(Debug)]
pub struct Roots<F: ManagedFile> {
    data: Arc<Data<F>>,
}

#[derive(Debug)]
struct Data<F: ManagedFile> {
    context: Context<F::Manager>,
    transactions: TransactionManager<F::Manager>,
    thread_pool: ThreadPool<F>,
    path: PathBuf,
    tree_states: Mutex<HashMap<String, Box<dyn AnyTreeState>>>,
}

impl<F: ManagedFile> Roots<F> {
    fn open<P: Into<PathBuf> + Send>(path: P, context: Context<F::Manager>) -> Result<Self, Error> {
        let path = path.into();
        if !path.exists() {
            fs::create_dir_all(&path)?;
        } else if !path.is_dir() {
            return Err(Error::message(format!(
                "'{:?}' already exists, but is not a directory.",
                path
            )));
        }

        let transactions = TransactionManager::spawn(&path, context.clone())?;
        Ok(Self {
            data: Arc::new(Data {
                context,
                path,
                transactions,
                thread_pool: ThreadPool::default(),
                tree_states: Mutex::default(),
            }),
        })
    }

    /// Returns the path to the database directory.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.data.path
    }

    /// Returns the vault used to encrypt this database.
    #[must_use]
    pub fn context(&self) -> &Context<F::Manager> {
        &self.data.context
    }

    /// Returns the transaction manager for this database.
    #[must_use]
    pub fn transactions(&self) -> &TransactionManager<F::Manager> {
        &self.data.transactions
    }

    /// Opens a tree named `name`.
    // TODO enforce name restrictions.
    pub fn tree<Root: tree::Root, Name: Into<Cow<'static, str>>>(
        &self,
        name: Name,
    ) -> Result<Tree<Root, F>, Error> {
        let name = name.into();
        let path = self.tree_path(&name);
        if !path.exists() {
            self.context().file_manager.append(&path)?;
        }
        let state = self.tree_state(name.clone());
        Ok(Tree {
            roots: self.clone(),
            state,
            name,
        })
    }

    fn tree_path(&self, name: &str) -> PathBuf {
        self.path().join(format!("{}.nebari", name))
    }

    /// Removes a tree. Returns true if a tree was deleted.
    pub fn delete_tree(&self, name: impl Into<Cow<'static, str>>) -> Result<bool, Error> {
        let name = name.into();
        let mut tree_states = self.data.tree_states.lock();
        self.context()
            .file_manager
            .delete(self.tree_path(name.as_ref()))?;
        Ok(tree_states.remove(name.as_ref()).is_some())
    }

    /// Returns a list of all the names of trees contained in this database.
    pub fn tree_names(&self) -> Result<Vec<String>, Error> {
        let mut names = Vec::new();
        for entry in std::fs::read_dir(self.path())? {
            let entry = entry?;
            if let Some(name) = entry.file_name().to_str() {
                if let Some(without_extension) = name.strip_suffix(".nebari") {
                    names.push(without_extension.to_string());
                }
            }
        }
        Ok(names)
    }

    fn tree_state<Root: tree::Root>(&self, name: impl Into<Cow<'static, str>>) -> State<Root> {
        self.tree_states(&[Root::tree(name)])
            .into_iter()
            .next()
            .unwrap()
            .as_ref()
            .as_any()
            .downcast_ref::<State<Root>>()
            .unwrap()
            .clone()
    }

    fn tree_states(&self, names: &[TreeRoot<F>]) -> Vec<Box<dyn AnyTreeState>> {
        let mut tree_states = self.data.tree_states.lock();
        let mut output = Vec::with_capacity(names.len());
        for tree in names {
            let state = tree_states
                .entry(tree.name().to_string())
                .or_insert_with(|| tree.default_state())
                .cloned();
            output.push(state);
        }
        output
    }

    /// Begins a transaction over `trees`. All trees will be exclusively
    /// accessible by the transaction. Dropping the executing transaction will
    /// roll the transaction back.
    pub fn transaction(&self, trees: &[TreeRoot<F>]) -> Result<ExecutingTransaction<F>, Error> {
        // TODO this extra vec here is annoying. We should have a treename type
        // that we can use instead of str.
        let transaction = self.data.transactions.new_transaction(
            &trees
                .iter()
                .map(|t| t.name().as_bytes())
                .collect::<Vec<_>>(),
        );
        let states = self.tree_states(trees);
        let trees = trees
            .iter()
            .zip(states.into_iter())
            .map(|(tree, state)| {
                tree.begin_transaction(
                    transaction.id,
                    &self.tree_path(tree.name()),
                    state.as_ref(),
                    self.context(),
                    Some(&self.data.transactions),
                )
            })
            .collect::<Result<Vec<_>, Error>>()?;
        Ok(ExecutingTransaction {
            roots: self.clone(),
            transaction: Some(transaction),
            trees,
            transaction_manager: self.data.transactions.clone(),
        })
    }
}

impl<M: ManagedFile> Clone for Roots<M> {
    fn clone(&self) -> Self {
        Self {
            data: self.data.clone(),
        }
    }
}

/// An executing transaction. While this exists, no other transactions can
/// execute across the same trees as this transaction holds.
#[must_use]
pub struct ExecutingTransaction<F: ManagedFile> {
    roots: Roots<F>,
    transaction_manager: TransactionManager<F::Manager>,
    trees: Vec<Box<dyn AnyTransactionTree<F>>>,
    transaction: Option<TransactionHandle>,
}

impl<F: ManagedFile> ExecutingTransaction<F> {
    /// Commits the transaction. Once this function has returned, all data
    /// updates are guaranteed to be able to be accessed by all other readers as
    /// well as impervious to sudden failures such as a power outage.
    #[allow(clippy::missing_panics_doc)]
    pub fn commit(mut self) -> Result<(), Error> {
        let trees = std::mem::take(&mut self.trees);
        let trees = self.roots.data.thread_pool.commit_trees(trees)?;

        self.transaction_manager
            .push(self.transaction.take().unwrap())?;

        for tree in trees {
            tree.publish();
        }

        Ok(())
    }

    /// Accesses a locked tree. The order of `TransactionTree`'s
    pub fn tree<Root: tree::Root>(
        &mut self,
        index: usize,
    ) -> Option<&mut TransactionTree<Root, F>> {
        self.trees
            .get_mut(index)
            .and_then(|any_tree| any_tree.as_mut().as_any_mut().downcast_mut())
    }

    fn rollback_tree_states(&mut self) {
        for tree in self.trees.drain(..) {
            tree.rollback();
        }
    }
}

impl<F: ManagedFile> Drop for ExecutingTransaction<F> {
    fn drop(&mut self) {
        if let Some(transaction) = self.transaction.take() {
            self.rollback_tree_states();
            self.trees.clear();
            // Now the transaction can be dropped safely, freeing up access to the trees.
            drop(transaction);
        }
    }
}

/// A tree that is modifiable during a transaction.
pub struct TransactionTree<Root: tree::Root, F: ManagedFile> {
    pub(crate) transaction_id: u64,
    pub(crate) tree: TreeFile<Root, F>,
}

pub trait AnyTransactionTree<F: ManagedFile>: Any + Send + Sync {
    fn as_any(&self) -> &dyn Any;
    fn as_any_mut(&mut self) -> &mut dyn Any;

    fn state(&self) -> Box<dyn AnyTreeState>;

    fn commit(&mut self) -> Result<(), Error>;
    fn rollback(&self);
}

impl<Root: tree::Root, F: ManagedFile> AnyTransactionTree<F> for TransactionTree<Root, F> {
    fn as_any(&self) -> &dyn Any {
        self
    }
    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }

    fn state(&self) -> Box<dyn AnyTreeState> {
        Box::new(self.tree.state.clone())
    }

    fn commit(&mut self) -> Result<(), Error> {
        self.tree.commit()?;
        Ok(())
    }

    fn rollback(&self) {
        let mut state = self.tree.state.lock();
        state.rollback(&self.tree.state);
    }
}

impl<F: ManagedFile> TransactionTree<VersionedTreeRoot, F> {
    /// Returns the latest sequence id.
    pub fn current_sequence_id(&self) -> u64 {
        let state = self.tree.state.lock();
        state.header.sequence
    }
}

impl<Root: tree::Root, F: ManagedFile> TransactionTree<Root, F> {
    /// Sets `key` to `value`.
    pub fn set(
        &mut self,
        key: impl Into<Buffer<'static>>,
        value: impl Into<Buffer<'static>>,
    ) -> Result<(), Error> {
        self.tree
            .modify(Modification {
                transaction_id: self.transaction_id,
                keys: vec![key.into()],
                operation: Operation::Set(value.into()),
            })
            .map(|_| {})
    }

    /// Sets `key` to `value`. If a value already exists, it will be returned.
    #[allow(clippy::missing_panics_doc)]
    pub fn replace(
        &mut self,
        key: impl Into<Buffer<'static>>,
        value: impl Into<Buffer<'static>>,
    ) -> Result<Option<Buffer<'static>>, Error> {
        let mut existing_value = None;
        let mut value = Some(value.into());
        self.tree.modify(Modification {
            transaction_id: self.transaction_id,
            keys: vec![key.into()],
            operation: Operation::CompareSwap(CompareSwap::new(&mut |_, stored_value| {
                existing_value = stored_value;
                KeyOperation::Set(value.take().unwrap())
            })),
        })?;

        Ok(existing_value)
    }

    /// Returns the current value of `key`. This will return updated information
    /// if it has been previously updated within this transaction.
    pub fn get(&mut self, key: &[u8]) -> Result<Option<Buffer<'static>>, Error> {
        self.tree.get(key, true)
    }

    /// Removes `key` and returns the existing value, if present.
    pub fn remove(&mut self, key: &[u8]) -> Result<Option<Buffer<'static>>, Error> {
        let mut existing_value = None;
        self.tree.modify(Modification {
            transaction_id: self.transaction_id,
            keys: vec![Buffer::from(key)],
            operation: Operation::CompareSwap(CompareSwap::new(&mut |_key, value| {
                existing_value = value;
                KeyOperation::Remove
            })),
        })?;
        Ok(existing_value)
    }

    /// Compares the value of `key` against `old`. If the values match, key will
    /// be set to the new value if `new` is `Some` or removed if `new` is
    /// `None`.
    pub fn compare_and_swap(
        &mut self,
        key: &[u8],
        old: Option<&Buffer<'_>>,
        mut new: Option<Buffer<'_>>,
    ) -> Result<(), CompareAndSwapError> {
        let mut result = Ok(());
        self.tree.modify(Modification {
            transaction_id: self.transaction_id,
            keys: vec![Buffer::from(key)],
            operation: Operation::CompareSwap(CompareSwap::new(&mut |_key, value| {
                if value.as_ref() == old {
                    match new.take() {
                        Some(new) => KeyOperation::Set(new.to_owned()),
                        None => KeyOperation::Remove,
                    }
                } else {
                    result = Err(CompareAndSwapError::Conflict(value));
                    KeyOperation::Skip
                }
            })),
        })?;
        result
    }

    /// Retrieves the values of `keys`. If any keys are not found, they will be
    /// omitted from the results.
    pub fn get_multiple(
        &mut self,
        keys: &[&[u8]],
    ) -> Result<Vec<(Buffer<'static>, Buffer<'static>)>, Error> {
        self.tree.get_multiple(keys, true)
    }

    /// Retrieves all of the values of keys within `range`.
    pub fn get_range<'b, B: RangeBounds<Buffer<'b>> + Debug + 'static>(
        &mut self,
        range: B,
    ) -> Result<Vec<(Buffer<'static>, Buffer<'static>)>, Error> {
        self.tree.get_range(range, true)
    }

    /// Scans the tree. Each key that is contained `range` will be passed to
    /// `key_evaluator`, which can opt to read the data for the key, skip, or
    /// stop scanning. If `KeyEvaluation::ReadData` is returned, `callback` will
    /// be invoked with the key and stored value. The order in which `callback`
    /// is invoked is not necessarily the same order in which the keys are
    /// found.
    #[cfg_attr(
        feature = "tracing",
        tracing::instrument(skip(self, key_evaluator, callback))
    )]
    pub fn scan<'b, E, B, KeyEvaluator, DataCallback>(
        &mut self,
        range: B,
        forwards: bool,
        key_evaluator: KeyEvaluator,
        callback: DataCallback,
    ) -> Result<(), AbortError<E>>
    where
        B: RangeBounds<Buffer<'b>> + Debug + 'static,
        KeyEvaluator: FnMut(&Buffer<'static>) -> KeyEvaluation,
        DataCallback: FnMut(Buffer<'static>, Buffer<'static>) -> Result<(), AbortError<E>>,
        E: Display + Debug,
    {
        self.tree
            .scan(range, forwards, true, key_evaluator, callback)
    }

    /// Returns the last  of the tree.
    #[cfg_attr(feature = "tracing", tracing::instrument(skip(self)))]
    pub fn last_key(&mut self) -> Result<Option<Buffer<'static>>, Error> {
        let mut result = None;
        self.tree
            .scan(
                ..,
                false,
                false,
                |key| {
                    result = Some(key.clone());
                    KeyEvaluation::Stop
                },
                |_key, _value| Ok(()),
            )
            .map_err(AbortError::infallible)?;

        Ok(result)
    }

    /// Returns the last key and value of the tree.
    #[cfg_attr(feature = "tracing", tracing::instrument(skip(self)))]
    pub fn last(&mut self) -> Result<Option<(Buffer<'static>, Buffer<'static>)>, Error> {
        let mut result = None;
        let mut key_requested = false;
        self.tree
            .scan(
                ..,
                false,
                false,
                |_| {
                    if key_requested {
                        KeyEvaluation::Stop
                    } else {
                        key_requested = true;
                        KeyEvaluation::ReadData
                    }
                },
                |key, value| {
                    result = Some((key, value));
                    Ok(())
                },
            )
            .map_err(AbortError::infallible)?;

        Ok(result)
    }
}

/// An error returned from `compare_and_swap()`.
#[derive(Debug, thiserror::Error)]
pub enum CompareAndSwapError {
    /// The stored value did not match the conditional value.
    #[error("value did not match. existing value: {0:?}")]
    Conflict(Option<Buffer<'static>>),
    /// Another error occurred while executing the operation.
    #[error("error during compare_and_swap: {0}")]
    Error(#[from] Error),
}

/// A database configuration used to open a database.
#[derive(Debug)]
#[must_use]
pub struct Config<F: ManagedFile> {
    path: PathBuf,
    vault: Option<Arc<dyn Vault>>,
    cache: Option<ChunkCache>,
    _file: PhantomData<F>,
}

impl<F: ManagedFile> Config<F> {
    /// Creates a new config to open a database located at `path`.
    pub fn new<P: AsRef<Path>>(path: P) -> Self {
        Self {
            path: path.as_ref().to_path_buf(),
            vault: None,
            cache: None,
            _file: PhantomData,
        }
    }

    /// Sets the vault to use for this database.
    pub fn vault<V: Vault>(mut self, vault: V) -> Self {
        self.vault = Some(Arc::new(vault));
        self
    }

    /// Sets the chunk cache to use for this database.
    pub fn cache(mut self, cache: ChunkCache) -> Self {
        self.cache = Some(cache);
        self
    }

    /// Opens the database, or creates one if the target path doesn't exist.
    pub fn open(self) -> Result<Roots<F>, Error> {
        Roots::open(
            self.path,
            Context {
                file_manager: F::Manager::default(),
                vault: self.vault,
                cache: self.cache,
            },
        )
    }
}

/// A named collection of keys and values.
pub struct Tree<Root: tree::Root, F: ManagedFile> {
    roots: Roots<F>,
    state: State<Root>,
    name: Cow<'static, str>,
}

impl<Root: tree::Root, F: ManagedFile> Tree<Root, F> {
    /// Returns the name of the tree.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Returns the path to the file for this tree.
    #[must_use]
    pub fn path(&self) -> PathBuf {
        self.roots.tree_path(self.name())
    }

    /// Sets `key` to `value`. This is executed within its own transaction.
    #[allow(clippy::missing_panics_doc)]
    pub fn set(
        &self,
        key: impl Into<Buffer<'static>>,
        value: impl Into<Buffer<'static>>,
    ) -> Result<(), Error> {
        let mut transaction = self.roots.transaction(&[Root::tree(self.name.clone())])?;
        transaction.tree::<Root>(0).unwrap().set(key, value)?;
        transaction.commit()
    }

    /// Retrieves the current value of `key`, if present. Does not reflect any
    /// changes in pending transactions.
    pub fn get(&self, key: &[u8]) -> Result<Option<Buffer<'static>>, Error> {
        let mut tree = TreeFile::<Root, F>::read(
            self.path(),
            self.state.clone(),
            self.roots.context(),
            Some(self.roots.transactions()),
        )?;

        tree.get(key, false)
    }

    /// Removes `key` and returns the existing value, if present. This is executed within its own transaction.
    #[allow(clippy::missing_panics_doc)]
    pub fn remove(&self, key: &[u8]) -> Result<Option<Buffer<'static>>, Error> {
        let mut transaction = self.roots.transaction(&[Root::tree(self.name.clone())])?;
        let existing_value = transaction.tree::<Root>(0).unwrap().remove(key)?;
        transaction.commit()?;
        Ok(existing_value)
    }

    /// Compares the value of `key` against `old`. If the values match, key will
    /// be set to the new value if `new` is `Some` or removed if `new` is
    /// `None`. This is executed within its own transaction.
    #[allow(clippy::missing_panics_doc)]
    pub fn compare_and_swap(
        &self,
        key: &[u8],
        old: Option<&Buffer<'_>>,
        new: Option<Buffer<'_>>,
    ) -> Result<(), CompareAndSwapError> {
        let mut transaction = self.roots.transaction(&[Root::tree(self.name.clone())])?;
        transaction
            .tree::<Root>(0)
            .unwrap()
            .compare_and_swap(key, old, new)?;
        transaction.commit()?;
        Ok(())
    }

    /// Retrieves the values of `keys`. If any keys are not found, they will be
    /// omitted from the results.
    pub fn get_multiple(
        &self,
        keys: &[&[u8]],
    ) -> Result<Vec<(Buffer<'static>, Buffer<'static>)>, Error> {
        let mut tree = TreeFile::<Root, F>::read(
            self.path(),
            self.state.clone(),
            self.roots.context(),
            Some(self.roots.transactions()),
        )?;

        tree.get_multiple(keys, false)
    }

    /// Retrieves all of the values of keys within `range`.
    pub fn get_range<'b, B: RangeBounds<Buffer<'b>> + Debug + 'static>(
        &self,
        range: B,
    ) -> Result<Vec<(Buffer<'static>, Buffer<'static>)>, Error> {
        let mut tree = TreeFile::<Root, F>::read(
            self.path(),
            self.state.clone(),
            self.roots.context(),
            Some(self.roots.transactions()),
        )?;

        tree.get_range(range, false)
    }

    /// Scans the tree. Each key that is contained `range` will be passed to
    /// `key_evaluator`, which can opt to read the data for the key, skip, or
    /// stop scanning. If `KeyEvaluation::ReadData` is returned, `callback` will
    /// be invoked with the key and stored value. The order in which `callback`
    /// is invoked is not necessarily the same order in which the keys are
    /// found.
    #[cfg_attr(
        feature = "tracing",
        tracing::instrument(skip(self, key_evaluator, callback))
    )]
    pub fn scan<'b, E, B, KeyEvaluator, DataCallback>(
        &self,
        range: B,
        forwards: bool,
        key_evaluator: KeyEvaluator,
        callback: DataCallback,
    ) -> Result<(), AbortError<E>>
    where
        B: RangeBounds<Buffer<'b>> + Debug + 'static,
        KeyEvaluator: FnMut(&Buffer<'static>) -> KeyEvaluation,
        DataCallback: FnMut(Buffer<'static>, Buffer<'static>) -> Result<(), AbortError<E>>,
        E: Display + Debug,
    {
        let mut tree = TreeFile::<Root, F>::read(
            self.path(),
            self.state.clone(),
            self.roots.context(),
            Some(self.roots.transactions()),
        )?;

        tree.scan(range, forwards, false, key_evaluator, callback)
    }

    /// Returns the last key of the tree.
    #[cfg_attr(feature = "tracing", tracing::instrument(skip(self)))]
    pub fn last_key(&self) -> Result<Option<Buffer<'static>>, Error> {
        let mut tree = TreeFile::<Root, F>::read(
            self.path(),
            self.state.clone(),
            self.roots.context(),
            Some(self.roots.transactions()),
        )?;

        let mut result = None;
        tree.scan(
            ..,
            false,
            false,
            |key| {
                result = Some(key.clone());
                KeyEvaluation::Stop
            },
            |_key, _value| Ok(()),
        )
        .map_err(AbortError::infallible)?;

        Ok(result)
    }

    /// Returns the last key and value of the tree.
    #[cfg_attr(feature = "tracing", tracing::instrument(skip(self)))]
    pub fn last(&self) -> Result<Option<(Buffer<'static>, Buffer<'static>)>, Error> {
        let mut tree = TreeFile::<Root, F>::read(
            self.path(),
            self.state.clone(),
            self.roots.context(),
            Some(self.roots.transactions()),
        )?;

        let mut result = None;
        let mut key_requested = false;
        tree.scan(
            ..,
            false,
            false,
            |_| {
                if key_requested {
                    KeyEvaluation::Stop
                } else {
                    key_requested = true;
                    KeyEvaluation::ReadData
                }
            },
            |key, value| {
                result = Some((key, value));
                Ok(())
            },
        )
        .map_err(AbortError::infallible)?;

        Ok(result)
    }
}

/// An error that could come from user code or Roots.
#[derive(thiserror::Error, Debug)]
pub enum AbortError<U: Display + Debug> {
    /// An error unrelated to Roots occurred.
    #[error("other error: {0}")]
    Other(U),
    /// An error from Roots occurred.
    #[error("database error: {0}")]
    Roots(#[from] Error),
}

impl AbortError<Infallible> {
    /// Unwraps the error contained within an infallible abort error.
    #[must_use]
    pub fn infallible(self) -> Error {
        match self {
            AbortError::Other(_) => unreachable!(),
            AbortError::Roots(error) => error,
        }
    }
}

#[derive(Debug)]
struct ThreadPool<F>
where
    F: ManagedFile,
{
    sender: flume::Sender<ThreadCommit<F>>,
    receiver: flume::Receiver<ThreadCommit<F>>,
    thread_count: AtomicU16,
}

impl<F: ManagedFile> ThreadPool<F> {
    pub fn commit_trees(
        &self,
        mut trees: Vec<Box<dyn AnyTransactionTree<F>>>,
    ) -> Result<Vec<Box<dyn AnyTreeState>>, Error> {
        static CPU_COUNT: Lazy<usize> = Lazy::new(num_cpus::get);

        if trees.len() == 1 {
            trees[0].commit()?;
            Ok(vec![trees[0].state()])
        } else {
            // Push the trees so that any existing threads can begin processing the queue.
            let (completion_sender, completion_receiver) = flume::unbounded();
            let tree_count = trees.len();
            for tree in trees {
                self.sender.send(ThreadCommit {
                    tree,
                    completion_sender: completion_sender.clone(),
                })?;
            }

            // Scale the queue if needed.
            let desired_threads = tree_count.min(*CPU_COUNT);
            loop {
                let thread_count = self.thread_count.load(Ordering::SeqCst);
                if (thread_count as usize) >= desired_threads {
                    break;
                }

                // Spawn a thread, but ensure that we don't spin up too many threads if another thread is committing at the same time.
                if self
                    .thread_count
                    .compare_exchange(
                        thread_count,
                        thread_count + 1,
                        Ordering::SeqCst,
                        Ordering::SeqCst,
                    )
                    .is_ok()
                {
                    let commit_receiver = self.receiver.clone();
                    std::thread::Builder::new()
                        .name(String::from("roots-txwriter"))
                        .spawn(move || transaction_commit_thread(commit_receiver))
                        .unwrap();
                }
            }

            // Wait for our results
            let mut results = Vec::with_capacity(tree_count);
            for _ in 0..tree_count {
                results.push(completion_receiver.recv()??);
            }

            Ok(results)
        }
    }
}

#[allow(clippy::needless_pass_by_value)]
fn transaction_commit_thread<F: ManagedFile>(receiver: flume::Receiver<ThreadCommit<F>>) {
    while let Ok(ThreadCommit {
        mut tree,
        completion_sender,
    }) = receiver.recv()
    {
        let result = tree.commit().map(move |_| tree.state());
        drop(completion_sender.send(result));
    }
}

impl<F: ManagedFile> Default for ThreadPool<F> {
    fn default() -> Self {
        let (sender, receiver) = flume::unbounded();
        Self {
            sender,
            receiver,
            thread_count: AtomicU16::new(0),
        }
    }
}

struct ThreadCommit<F>
where
    F: ManagedFile,
{
    tree: Box<dyn AnyTransactionTree<F>>,
    completion_sender: Sender<Result<Box<dyn AnyTreeState>, Error>>,
}

#[cfg(test)]
mod tests {
    use tempfile::tempdir;

    use super::*;
    use crate::{managed_file::memory::MemoryFile, tree::Root, ManagedFile, StdFile};

    fn basic_get_set<F: ManagedFile>() {
        let tempdir = tempdir().unwrap();
        let roots = Config::<F>::new(tempdir.path()).open().unwrap();

        let tree = roots.tree::<VersionedTreeRoot, _>("test").unwrap();
        tree.set(b"test", b"value").unwrap();
        let result = tree.get(b"test").unwrap().expect("key not found");

        assert_eq!(result.as_slice(), b"value");
    }

    #[test]
    fn memory_basic_get_set() {
        basic_get_set::<MemoryFile>();
    }

    #[test]
    fn std_basic_get_set() {
        basic_get_set::<StdFile>();
    }

    #[test]
    fn basic_transaction_isolation_test() {
        let tempdir = tempdir().unwrap();

        let roots = Config::<StdFile>::new(tempdir.path()).open().unwrap();
        let tree = roots.tree::<VersionedTreeRoot, _>("test").unwrap();
        tree.set(b"test", b"value").unwrap();

        // Begin a transaction
        let mut transaction = roots
            .transaction(&[VersionedTreeRoot::tree("test")])
            .unwrap();

        // Replace the key with a new value.
        transaction
            .tree::<VersionedTreeRoot>(0)
            .unwrap()
            .set(b"test", b"updated value")
            .unwrap();

        // Check that the transaction can read the new value
        let result = transaction
            .tree::<VersionedTreeRoot>(0)
            .unwrap()
            .get(b"test")
            .unwrap()
            .expect("key not found");
        assert_eq!(result.as_slice(), b"updated value");

        // Ensure that existing read-access doesn't see the new value
        let result = tree.get(b"test").unwrap().expect("key not found");
        assert_eq!(result.as_slice(), b"value");

        // Commit the transaction
        transaction.commit().unwrap();

        // Ensure that the reader now sees the new value
        let result = tree.get(b"test").unwrap().expect("key not found");
        assert_eq!(result.as_slice(), b"updated value");
    }

    #[test]
    fn basic_transaction_rollback_test() {
        let tempdir = tempdir().unwrap();

        let roots = Config::<StdFile>::new(tempdir.path()).open().unwrap();
        let tree = roots.tree::<VersionedTreeRoot, _>("test").unwrap();
        tree.set(b"test", b"value").unwrap();

        // Begin a transaction
        let mut transaction = roots
            .transaction(&[VersionedTreeRoot::tree("test")])
            .unwrap();

        // Replace the key with a new value.
        transaction
            .tree::<VersionedTreeRoot>(0)
            .unwrap()
            .set(b"test", b"updated value")
            .unwrap();

        // Roll the transaction back
        drop(transaction);

        // Ensure that the reader still sees the old value
        let result = tree.get(b"test").unwrap().expect("key not found");
        assert_eq!(result.as_slice(), b"value");

        // Begin a new transaction
        let mut transaction = roots
            .transaction(&[VersionedTreeRoot::tree("test")])
            .unwrap();
        // Check that the transaction has the original value
        let result = transaction
            .tree::<VersionedTreeRoot>(0)
            .unwrap()
            .get(b"test")
            .unwrap()
            .expect("key not found");
        assert_eq!(result.as_slice(), b"value");
    }
}
