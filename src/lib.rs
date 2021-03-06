//! Deadpool is a dead simple async pool for connections and objects
//! of any type.
//!
//! # Example
//!
//! ```rust
//! use async_trait::async_trait;
//!
//! #[derive(Debug)]
//! enum Error { Fail }
//!
//! struct Connection {}
//!
//! type Pool = deadpool::Pool<Connection, Error>;
//!
//! impl Connection {
//!     async fn new() -> Result<Self, Error> {
//!         Ok(Connection {})
//!     }
//!     async fn check_health(&self) -> bool {
//!         true
//!     }
//!     async fn do_something(&self) -> String {
//!         "Hooray!".to_string()
//!     }
//! }
//!
//! struct Manager {}
//!
//! #[async_trait]
//! impl deadpool::Manager<Connection, Error> for Manager
//! {
//!     async fn create(&self) -> Result<Connection, Error> {
//!         Connection::new().await
//!     }
//!     async fn recycle(&self, conn: &mut Connection) -> deadpool::RecycleResult<Error> {
//!         if conn.check_health().await {
//!             Ok(())
//!         } else {
//!             Err(Error::Fail.into())
//!         }
//!     }
//! }
//!
//! #[tokio::main]
//! async fn main() {
//!     let mgr = Manager {};
//!     let pool = Pool::new(mgr, 16);
//!     let mut conn = pool.get().await.unwrap();
//!     let value = conn.do_something().await;
//!     assert_eq!(value, "Hooray!".to_string());
//! }
//! ```
//!
//! For a more complete example please see
//! [`deadpool-postgres`](https://crates.io/crates/deadpool-postgres)
#![warn(missing_docs)]

use std::future::Future;
use std::ops::{Deref, DerefMut};
use std::sync::atomic::{AtomicIsize, AtomicUsize, Ordering};
use std::sync::{Arc, Weak};
use std::time::Duration;

use async_trait::async_trait;
use crossbeam_queue::ArrayQueue;
use tokio::sync::Semaphore;
use tokio::time::timeout;

mod config;
pub use config::{PoolConfig, Timeouts};
mod errors;
pub use errors::{PoolError, RecycleError, TimeoutType};
pub mod passive;

/// Result type for the recycle function
pub type RecycleResult<E> = Result<(), RecycleError<E>>;

/// This trait is used to `create` new objects or `recycle` existing ones.
#[async_trait]
pub trait Manager<T, E> {
    /// Create a new instance of `T`
    async fn create(&self) -> Result<T, E>;
    /// Try to recycle an instance of `T` returning None` if the
    /// object could not be recycled.
    async fn recycle(&self, obj: &mut T) -> RecycleResult<E>;
}

enum ObjectState {
    Waiting,
    Receiving,
    Creating,
    Recycling,
    Ready,
    Dropped,
}

/// A wrapper around the actual pooled object which implements the traits
/// `Deref`, `DerefMut` and `Drop`. Use this object just as if it was of type
/// `T` and upon leaving scope the `drop` function will take care of
/// returning it to the pool.
pub struct Object<T, E> {
    obj: Option<T>,
    state: ObjectState,
    pool: Weak<PoolInner<T, E>>,
}

impl<T, E> Drop for Object<T, E> {
    fn drop(&mut self) {
        if let Some(pool) = self.pool.upgrade() {
            match self.state {
                ObjectState::Waiting => {
                    pool.available.fetch_add(1, Ordering::Relaxed);
                }
                ObjectState::Receiving => {
                    pool.available.fetch_add(1, Ordering::Relaxed);
                    pool.semaphore.add_permits(1);
                }
                ObjectState::Creating => {
                    pool.size.fetch_sub(1, Ordering::Relaxed);
                    pool.semaphore.add_permits(1);
                }
                ObjectState::Recycling | ObjectState::Ready => {
                    pool.available.fetch_add(1, Ordering::Relaxed);
                    let obj = self.obj.take().unwrap();
                    pool.queue.push(obj).unwrap();
                    pool.semaphore.add_permits(1);
                }
                ObjectState::Dropped => {
                    // The object has already been dropped.
                }
            }
        }
        self.obj = None;
        self.state = ObjectState::Dropped;
    }
}

impl<T, E> Deref for Object<T, E> {
    type Target = T;
    fn deref(&self) -> &T {
        self.obj.as_ref().unwrap()
    }
}

impl<T, E> DerefMut for Object<T, E> {
    fn deref_mut(&mut self) -> &mut T {
        self.obj.as_mut().unwrap()
    }
}

struct PoolInner<T, E> {
    manager: Box<dyn Manager<T, E> + Sync + Send>,
    queue: ArrayQueue<T>,
    size: AtomicUsize,
    /// The number of available objects in the pool. If there are no
    /// objects in the pool this number can become negative and stores the
    /// number of futures waiting for an object.
    available: AtomicIsize,
    semaphore: Semaphore,
    config: PoolConfig,
}

/// A generic object and connection pool.
///
/// This struct can be cloned and transferred across thread boundaries
/// and uses reference counting for its internal state.
pub struct Pool<T, E> {
    inner: Arc<PoolInner<T, E>>,
}

#[derive(Debug)]
/// The current pool status.
pub struct Status {
    /// The size of the pool
    pub size: usize,
    /// The number of available objects in the pool. If there are no
    /// objects in the pool this number can become negative and stores the
    /// number of futures waiting for an object.
    pub available: isize,
}

impl<T, E> Clone for Pool<T, E> {
    fn clone(&self) -> Pool<T, E> {
        Pool {
            inner: self.inner.clone(),
        }
    }
}

impl<T, E> Pool<T, E> {
    /// Create new connection pool with a given `manager` and `max_size`.
    /// The `manager` is used to create and recycle objects and `max_size`
    /// is the maximum number of objects ever created.
    pub fn new(manager: impl Manager<T, E> + Send + Sync + 'static, max_size: usize) -> Pool<T, E> {
        Self::from_config(manager, PoolConfig::new(max_size))
    }
    /// Create new connection pool with a given `manager` and `config`.
    /// The `manager` is used to create and recycle objects and `max_size`
    /// is the maximum number of objects ever created.
    pub fn from_config(
        manager: impl Manager<T, E> + Send + Sync + 'static,
        config: PoolConfig,
    ) -> Pool<T, E> {
        Pool {
            inner: Arc::new(PoolInner {
                manager: Box::new(manager),
                queue: ArrayQueue::new(config.max_size),
                size: AtomicUsize::new(0),
                available: AtomicIsize::new(0),
                semaphore: Semaphore::new(config.max_size),
                config: config,
            }),
        }
    }
    /// Retrieve object from pool or wait for one to become available.
    pub async fn get(&self) -> Result<Object<T, E>, PoolError<E>> {
        self.timeout_get(&self.inner.config.timeouts).await
    }
    /// Retrieve object from the pool and do not wait if there is currently
    /// no object available and the maximum pool size has been reached.
    pub async fn try_get(&self) -> Result<Object<T, E>, PoolError<E>> {
        let mut timeouts = self.inner.config.timeouts.clone();
        timeouts.wait = Some(Duration::from_secs(0));
        self.timeout_get(&timeouts).await
    }
    /// Retrieve object using a different timeout config than the one
    /// configured.
    pub async fn timeout_get(&self, timeouts: &Timeouts) -> Result<Object<T, E>, PoolError<E>> {
        self.inner.available.fetch_sub(1, Ordering::Relaxed);

        let mut obj = Object {
            obj: None,
            state: ObjectState::Waiting,
            pool: Arc::downgrade(&self.inner),
        };

        let non_blocking = match timeouts.wait {
            Some(t) => t.as_nanos() == 0,
            None => false,
        };

        let permit = if non_blocking {
            self.inner
                .semaphore
                .try_acquire()
                .map_err(|_| PoolError::Timeout(TimeoutType::Wait))?
        } else {
            apply_timeout(
                self.inner.semaphore.acquire(),
                TimeoutType::Wait,
                self.inner.config.timeouts.wait,
            )
            .await?
        };

        permit.forget();

        loop {
            obj.state = ObjectState::Receiving;
            match self.inner.queue.pop() {
                Ok(inner_obj) => {
                    // Recycle existing object
                    obj.state = ObjectState::Recycling;
                    obj.obj = Some(inner_obj);
                    match apply_timeout(
                        self.inner.manager.recycle(&mut obj),
                        TimeoutType::Recycle,
                        self.inner.config.timeouts.recycle,
                    )
                    .await?
                    {
                        Ok(_) => break,
                        Err(_) => continue,
                    }
                }
                Err(_) => {
                    // Create new object
                    obj.state = ObjectState::Creating;
                    self.inner.available.fetch_add(1, Ordering::Relaxed);
                    self.inner.size.fetch_add(1, Ordering::Relaxed);
                    obj.obj = Some(
                        apply_timeout(
                            self.inner.manager.create(),
                            TimeoutType::Create,
                            self.inner.config.timeouts.create,
                        )
                        .await??,
                    );
                    break;
                }
            }
        }

        obj.state = ObjectState::Ready;
        Ok(obj)
    }
    /// Retrieve status of the pool
    pub fn status(&self) -> Status {
        let size = self.inner.size.load(Ordering::Relaxed);
        let available = self.inner.available.load(Ordering::Relaxed);
        Status { size, available }
    }
}

async fn apply_timeout<F, O, E>(
    future: F,
    timeout_type: TimeoutType,
    duration: Option<Duration>,
) -> Result<O, PoolError<E>>
where
    F: Future<Output = O>,
{
    match duration {
        Some(duration) => match timeout(duration, future).await {
            Ok(result) => Ok(result),
            Err(_) => Err(PoolError::Timeout(timeout_type)),
        },
        None => Ok(future.await),
    }
}
