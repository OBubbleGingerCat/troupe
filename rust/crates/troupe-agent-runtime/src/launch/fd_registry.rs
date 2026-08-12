use std::cell::UnsafeCell;
use std::mem::{ManuallyDrop, MaybeUninit};
use std::ops::{Deref, DerefMut};
use std::os::fd::{AsRawFd, RawFd};
use std::pin::Pin;
use std::ptr;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::task::{Context, Poll};

use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

struct ForkFdRegistry {
    owner_pid: u32,
    mutex: UnsafeCell<libc::pthread_mutex_t>,
    fds: UnsafeCell<Vec<RawFd>>,
    exec_spawn_thread: AtomicUsize,
}

unsafe impl Sync for ForkFdRegistry {}

static REGISTRY: OnceLock<&'static ForkFdRegistry> = OnceLock::new();

unsafe extern "C" fn prepare_fork() {
    if let Some(registry) = REGISTRY.get() {
        if registry.is_exec_spawn_thread() {
            return;
        }
        unsafe {
            libc::pthread_mutex_lock(registry.mutex.get());
        }
    }
}

unsafe extern "C" fn parent_after_fork() {
    if let Some(registry) = REGISTRY.get() {
        if registry.is_exec_spawn_thread() {
            return;
        }
        unsafe {
            libc::pthread_mutex_unlock(registry.mutex.get());
        }
    }
}

unsafe extern "C" fn child_after_fork() {
    if let Some(registry) = REGISTRY.get() {
        if registry.is_exec_spawn_thread() {
            return;
        }
        let fds = unsafe { &mut *registry.fds.get() };
        for fd in fds.iter().copied() {
            unsafe {
                libc::close(fd);
            }
        }
        unsafe {
            fds.set_len(0);
            libc::pthread_mutex_unlock(registry.mutex.get());
        }
    }
}

impl ForkFdRegistry {
    fn global() -> &'static Self {
        REGISTRY.get_or_init(|| {
            let mut mutex = MaybeUninit::<libc::pthread_mutex_t>::uninit();
            let status = unsafe { libc::pthread_mutex_init(mutex.as_mut_ptr(), ptr::null()) };
            assert_eq!(status, 0, "fork FD registry mutex must initialize");
            let registry = Box::leak(Box::new(Self {
                owner_pid: std::process::id(),
                mutex: UnsafeCell::new(unsafe { mutex.assume_init() }),
                fds: UnsafeCell::new(Vec::new()),
                exec_spawn_thread: AtomicUsize::new(0),
            }));
            let status = unsafe {
                libc::pthread_atfork(
                    Some(prepare_fork),
                    Some(parent_after_fork),
                    Some(child_after_fork),
                )
            };
            assert_eq!(status, 0, "fork FD registry handlers must install");
            registry
        })
    }

    fn is_owner_process(&self) -> bool {
        self.owner_pid == std::process::id()
    }

    fn is_exec_spawn_thread(&self) -> bool {
        self.exec_spawn_thread.load(Ordering::Acquire) == unsafe { libc::pthread_self() as usize }
    }

    fn lock(&self) {
        let status = unsafe { libc::pthread_mutex_lock(self.mutex.get()) };
        assert_eq!(status, 0, "fork FD registry mutex must lock");
    }

    fn unlock(&self) {
        let status = unsafe { libc::pthread_mutex_unlock(self.mutex.get()) };
        debug_assert_eq!(status, 0);
    }

    fn with_fds<R>(&self, operation: impl FnOnce(&mut Vec<RawFd>) -> R) -> R {
        struct Unlock<'a>(&'a ForkFdRegistry);

        impl Drop for Unlock<'_> {
            fn drop(&mut self) {
                self.0.unlock();
            }
        }

        self.lock();
        let _unlock = Unlock(self);
        operation(unsafe { &mut *self.fds.get() })
    }

    fn register_locked(&self, fd: RawFd) {
        let fds = unsafe { &mut *self.fds.get() };
        assert!(!fds.contains(&fd), "a Troupe-owned FD is registered once");
        fds.push(fd);
    }

    fn register(&self, fd: RawFd) {
        assert!(
            self.is_owner_process(),
            "fork child cannot register parent FDs"
        );
        self.with_fds(|fds| {
            assert!(!fds.contains(&fd), "a Troupe-owned FD is registered once");
            fds.push(fd);
        });
    }

    fn unregister_and_drop<T>(&self, fd: RawFd, value: &mut ManuallyDrop<T>) {
        self.with_fds(|fds| {
            let index = fds
                .iter()
                .position(|registered| *registered == fd)
                .expect("a Troupe-owned FD remains registered until Drop");
            fds.swap_remove(index);
            unsafe {
                ManuallyDrop::drop(value);
            }
        });
    }
}

pub(crate) struct ForkExecGuard {
    registry: &'static ForkFdRegistry,
}

impl ForkExecGuard {
    pub(crate) fn begin() -> Self {
        let registry = ForkFdRegistry::global();
        assert!(
            registry.is_owner_process(),
            "fork child cannot spawn a parent-owned agent"
        );
        registry.lock();
        registry
            .exec_spawn_thread
            .store(unsafe { libc::pthread_self() as usize }, Ordering::Release);
        Self { registry }
    }

    pub(crate) fn track<T: AsRawFd>(&self, value: T) -> ForkTracked<T> {
        let fd = value.as_raw_fd();
        self.registry.register_locked(fd);
        ForkTracked {
            value: ManuallyDrop::new(value),
            fd,
            registry: self.registry,
        }
    }
}

impl Drop for ForkExecGuard {
    fn drop(&mut self) {
        self.registry.exec_spawn_thread.store(0, Ordering::Release);
        self.registry.unlock();
    }
}

pub(crate) struct ForkTracked<T: AsRawFd> {
    value: ManuallyDrop<T>,
    fd: RawFd,
    registry: &'static ForkFdRegistry,
}

impl<T: AsRawFd> ForkTracked<T> {
    pub(crate) fn new(value: T) -> Self {
        let fd = value.as_raw_fd();
        let registry = ForkFdRegistry::global();
        registry.register(fd);
        Self {
            value: ManuallyDrop::new(value),
            fd,
            registry,
        }
    }
}

impl<T: AsRawFd> Deref for ForkTracked<T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        &self.value
    }
}

impl<T: AsRawFd> DerefMut for ForkTracked<T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.value
    }
}

impl<T: AsRawFd> AsRawFd for ForkTracked<T> {
    fn as_raw_fd(&self) -> RawFd {
        self.fd
    }
}

impl<T: AsRawFd> Drop for ForkTracked<T> {
    fn drop(&mut self) {
        if self.registry.is_owner_process() {
            self.registry.unregister_and_drop(self.fd, &mut self.value);
        }
    }
}

impl<T: AsyncRead + AsRawFd + Unpin> AsyncRead for ForkTracked<T> {
    fn poll_read(
        self: Pin<&mut Self>,
        context: &mut Context<'_>,
        buffer: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        Pin::new(&mut **self.get_mut()).poll_read(context, buffer)
    }
}

impl<T: AsyncWrite + AsRawFd + Unpin> AsyncWrite for ForkTracked<T> {
    fn poll_write(
        self: Pin<&mut Self>,
        context: &mut Context<'_>,
        buffer: &[u8],
    ) -> Poll<Result<usize, std::io::Error>> {
        Pin::new(&mut **self.get_mut()).poll_write(context, buffer)
    }

    fn poll_flush(
        self: Pin<&mut Self>,
        context: &mut Context<'_>,
    ) -> Poll<Result<(), std::io::Error>> {
        Pin::new(&mut **self.get_mut()).poll_flush(context)
    }

    fn poll_shutdown(
        self: Pin<&mut Self>,
        context: &mut Context<'_>,
    ) -> Poll<Result<(), std::io::Error>> {
        Pin::new(&mut **self.get_mut()).poll_shutdown(context)
    }

    fn is_write_vectored(&self) -> bool {
        self.value.is_write_vectored()
    }

    fn poll_write_vectored(
        self: Pin<&mut Self>,
        context: &mut Context<'_>,
        buffers: &[std::io::IoSlice<'_>],
    ) -> Poll<Result<usize, std::io::Error>> {
        Pin::new(&mut **self.get_mut()).poll_write_vectored(context, buffers)
    }
}
