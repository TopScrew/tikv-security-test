// Copyright 2021 TiKV Project Authors. Licensed under Apache-2.0.
use std::{
    fs::{File, Metadata},
    io::{Read, Write},
    path::PathBuf,
    pin::Pin,
    process::{Command, Stdio},
    sync::Mutex,
    time::{Duration, UNIX_EPOCH},
};

use chrono::{offset::Local, DateTime};
use futures::{
    channel::oneshot::{self, Sender},
    future::BoxFuture,
    select,
    task::{Context, Poll},
    Future, FutureExt, Stream, StreamExt,
};
use lazy_static::lazy_static;
use pprof::protos::Message;
use regex::Regex;
use tempfile::{NamedTempFile, TempDir};
#[cfg(not(test))]
use tikv_alloc::{activate_prof, deactivate_prof, dump_prof};
use tikv_util::defer;

#[cfg(test)]
pub use self::test_utils::TEST_PROFILE_MUTEX;
#[cfg(test)]
use self::test_utils::{activate_prof, deactivate_prof, dump_prof};

// File name suffix for periodically dumped heap profiles.
pub const HEAP_PROFILE_SUFFIX: &str = ".heap";
pub const HEAP_PROFILE_REGEX: &str = r"^[0-9]{6,6}\.heap$";

lazy_static! {
    // If it's some it means there are already a CPU profiling.
    static ref CPU_PROFILE_ACTIVE: Mutex<Option<()>> = Mutex::new(None);
    // If it's some it means there are already a heap profiling. The channel is used to deactivate a profiling.
    pub static ref HEAP_PROFILE_ACTIVE: Mutex<Option<Option<(Sender<()>, TempDir)>>> = Mutex::new(None);

    // To normalize thread names.
    static ref THREAD_NAME_RE: Regex =
        Regex::new(r"^(?P<thread_name>[a-z-_ :]+?)(-?\d)*$").unwrap();
    static ref THREAD_NAME_REPLACE_SEPERATOR_RE: Regex = Regex::new(r"[_ ]").unwrap();
}

type OnEndFn<I, T> = Box<dyn FnOnce(I) -> Result<T, String> + Send + 'static>;

struct ProfileRunner<I, T> {
    item: Option<I>,
    on_end: Option<OnEndFn<I, T>>,
    end: BoxFuture<'static, Result<(), String>>,
}

impl<I, T> Unpin for ProfileRunner<I, T> {}

impl<I, T> ProfileRunner<I, T> {
    fn new<F1, F2>(
        on_start: F1,
        on_end: F2,
        end: BoxFuture<'static, Result<(), String>>,
    ) -> Result<Self, String>
    where
        F1: FnOnce() -> Result<I, String>,
        F2: FnOnce(I) -> Result<T, String> + Send + 'static,
    {
        let item = on_start()?;
        Ok(ProfileRunner {
            item: Some(item),
            on_end: Some(Box::new(on_end) as OnEndFn<I, T>),
            end,
        })
    }
}

impl<I, T> Future for ProfileRunner<I, T> {
    type Output = Result<T, String>;
    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        match self.end.as_mut().poll(cx) {
            Poll::Ready(res) => {
                let item = self.item.take().unwrap();
                let on_end = self.on_end.take().unwrap();
                let r = match (res, on_end(item)) {
                    (Ok(_), r) => r,
                    (Err(errmsg), _) => Err(errmsg),
                };
                Poll::Ready(r)
            }
            Poll::Pending => Poll::Pending,
        }
    }
}

/// Trigger a heap profile and return the content.
pub fn dump_one_heap_profile() -> Result<Vec<u8>, String> {
    if HEAP_PROFILE_ACTIVE.lock().unwrap().is_none() {
        return Err("heap profiling is not activated".to_owned());
    }
    let f = NamedTempFile::new().map_err(|e| format!("create tmp file fail: {}", e))?;
    let path = f.path().to_str().unwrap();
    dump_prof(path).map_err(|e| format!("dump_prof: {}", e))?;
    read_file(path)
}

/// Activate heap profile and call `callback` if successfully.
/// `deactivate_heap_profile` can only be called after it's notified from
/// `callback`.
pub async fn activate_heap_profile<S, F>(
    dump_period: Option<S>,
    store_path: PathBuf,
    callback: F,
) -> Result<(), String>
where
    S: Stream<Item = Result<(), String>> + Send + Unpin + 'static,
    F: FnOnce() + Send + 'static,
{
    if HEAP_PROFILE_ACTIVE.lock().unwrap().is_some() {
        return Err("Already in Heap Profiling".to_owned());
    }

    let (tx, rx) = oneshot::channel();
    let dir = tempfile::Builder::new()
        .prefix("heap-")
        .tempdir_in(store_path)
        .map_err(|e| format!("create temp directory: {}", e))?;
    let dir_path = dir.path().to_str().unwrap().to_owned();

    let on_start = move || {
        let mut activate = HEAP_PROFILE_ACTIVE.lock().unwrap();
        assert!(activate.is_none());
        *activate = Some(Some((tx, dir)));
        activate_prof().map_err(|e| format!("activate_prof: {}", e))?;
        callback();
        info!("periodical heap profiling is started");
        Ok(())
    };

    let on_end = |_| {
        let res = deactivate_prof().map_err(|e| format!("deactivate_prof: {}", e));
        *HEAP_PROFILE_ACTIVE.lock().unwrap() = None;
        res
    };

    let end = async move {
        if let Some(dump_period) = dump_period {
            select! {
                _ = rx.fuse() => {
                    info!("periodical heap profiling is canceled");
                    Ok(())
                },
                res = dump_heap_profile_periodically(dump_period, dir_path).fuse() => {
                    warn!("the heap profiling dump loop shouldn't break"; "res" => ?res);
                    res
                }
            }
        } else {
            let _ = rx.await;
            info!("periodical heap profiling is canceled");
            Ok(())
        }
    };

    ProfileRunner::new(on_start, on_end, end.boxed())?.await
}

/// Deactivate heap profile. Return `false` if it hasn't been activated.
pub fn deactivate_heap_profile() -> bool {
    let mut activate = HEAP_PROFILE_ACTIVE.lock().unwrap();
    match activate.as_mut() {
        Some(tx) => {
            if let Some((tx, _)) = tx.take() {
                let _ = tx.send(());
            } else {
                *activate = None;
            }
            true
        }
        None => false,
    }
}

/// Trigger one cpu profile.
pub async fn start_one_cpu_profile<F>(
    end: F,
    frequency: i32,
    protobuf: bool,
) -> Result<Vec<u8>, String>
where
    F: Future<Output = Result<(), String>> + Send + 'static,
{
    if CPU_PROFILE_ACTIVE.lock().unwrap().is_some() {
        return Err("Already in CPU Profiling".to_owned());
    }

    let on_start = || {
        let mut activate = CPU_PROFILE_ACTIVE.lock().unwrap();
        assert!(activate.is_none());
        *activate = Some(());
        let guard = pprof::ProfilerGuardBuilder::default()
            .frequency(frequency)
            .blocklist(&["libc", "libgcc", "pthread", "vdso"])
            .build()
            .map_err(|e| format!("pprof::ProfilerGuardBuilder::build fail: {}", e))?;
        Ok(guard)
    };

    let on_end = move |guard: pprof::ProfilerGuard<'static>| {
        defer! {
            *CPU_PROFILE_ACTIVE.lock().unwrap() = None
        }
        let report = guard
            .report()
            .frames_post_processor(move |frames| {
                let name = extract_thread_name(&frames.thread_name);
                frames.thread_name = name;
            })
            .build()
            .map_err(|e| format!("create cpu profiling report fail: {}", e))?;
        let mut body = Vec::new();
        if protobuf {
            let profile = report
                .pprof()
                .map_err(|e| format!("generate pprof from report fail: {}", e))?;
            profile
                .write_to_vec(&mut body)
                .map_err(|e| format!("encode pprof into bytes fail: {}", e))?;
        } else {
            report
                .flamegraph(&mut body)
                .map_err(|e| format!("generate flamegraph from report fail: {}", e))?;
        }
        drop(guard);

        Ok(body)
    };

    ProfileRunner::new(on_start, on_end, end.boxed())?.await
}

pub fn read_file(path: &str) -> Result<Vec<u8>, String> {
    let mut f = File::open(path).map_err(|e| format!("open {} fail: {}", path, e))?;
    let mut buf = Vec::new();
    f.read_to_end(&mut buf)
        .map_err(|e| format!("read {} fail: {}", path, e))?;
    Ok(buf)
}

pub fn jeprof_heap_profile(path: &str) -> Result<Vec<u8>, String> {
    info!("using jeprof to process {}", path);
    let bin = std::env::current_exe().map_err(|e| format!("get current exe path fail: {}", e))?;
    let mut jeprof = Command::new("perl")
        .args([
            "/dev/stdin",
            "--show_bytes",
            &bin.as_os_str().to_string_lossy(),
            path,
            "--svg",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("spawn jeprof fail: {}", e))?;
    jeprof
        .stdin
        .take()
        .unwrap()
        .write_all(include_bytes!("jeprof.in"))
        .unwrap();
    let output = jeprof
        .wait_with_output()
        .map_err(|e| format!("jeprof: {}", e))?;
    if !output.status.success() {
        let stderr = std::str::from_utf8(&output.stderr).unwrap_or("invalid utf8");
        return Err(format!("jeprof stderr: {:?}", stderr));
    }
    Ok(output.stdout)
}

pub fn heap_profiles_dir() -> Option<PathBuf> {
    HEAP_PROFILE_ACTIVE
        .lock()
        .unwrap()
        .as_ref()
        .and_then(|v| v.as_ref().map(|(_, dir)| dir.path().to_owned()))
}

pub fn list_heap_profiles() -> Result<Vec<(String, String)>, String> {
    let path = match heap_profiles_dir() {
        Some(path) => path.into_os_string().into_string().unwrap(),
        None => return Ok(vec![]),
    };

    let dir = std::fs::read_dir(path).map_err(|e| format!("read dir fail: {}", e))?;
    let mut profiles = Vec::new();
    for item in dir {
        let item = match item {
            Ok(x) => x,
            _ => continue,
        };
        let f = item.file_name().to_str().unwrap().to_owned();
        if !f.ends_with(HEAP_PROFILE_SUFFIX) {
            continue;
        }
        let ct = item.metadata().map(|x| last_change_epoch(&x)).unwrap();
        let dt = DateTime::<Local>::from(UNIX_EPOCH + Duration::from_secs(ct));
        profiles.push((f, dt.format("%Y-%m-%d %H:%M:%S").to_string()));
    }

    // Reverse sort them.
    profiles.sort_by(|x, y| y.1.cmp(&x.1));
    info!("list_heap_profiles gets {} items", profiles.len());
    Ok(profiles)
}

async fn dump_heap_profile_periodically<S>(mut period: S, dir: String) -> Result<(), String>
where
    S: Stream<Item = Result<(), String>> + Send + Unpin + 'static,
{
    let mut id = 0;
    while let Some(res) = period.next().await {
        res?;
        id += 1;
        let path = format!("{}/{:0>6}{}", dir, id, HEAP_PROFILE_SUFFIX);
        dump_prof(&path).map_err(|e| format!("dump_prof: {}", e))?;
        info!("a heap profile is dumped to {}", path);
    }
    Ok(())
}

fn extract_thread_name(thread_name: &str) -> String {
    THREAD_NAME_RE
        .captures(thread_name)
        .and_then(|cap| {
            cap.name("thread_name").map(|thread_name| {
                THREAD_NAME_REPLACE_SEPERATOR_RE
                    .replace_all(thread_name.as_str(), "-")
                    .into_owned()
            })
        })
        .unwrap_or_else(|| thread_name.to_owned())
}

// Re-define some heap profiling functions because heap-profiling is not enabled
// for tests.
#[cfg(test)]
mod test_utils {
    use std::sync::Mutex;

    use tikv_alloc::error::ProfResult;

    lazy_static! {
        pub static ref TEST_PROFILE_MUTEX: Mutex<()> = Mutex::new(());
    }

    pub fn activate_prof() -> ProfResult<()> {
        Ok(())
    }
    pub fn deactivate_prof() -> ProfResult<()> {
        Ok(())
    }
    pub fn dump_prof(_: &str) -> ProfResult<()> {
        Ok(())
    }
}

#[cfg(unix)]
fn last_change_epoch(metadata: &Metadata) -> u64 {
    use std::os::unix::fs::MetadataExt;
    metadata.ctime() as u64
}

#[cfg(not(unix))]
fn last_change_epoch(metadata: &Metadata) -> u64 {
    0
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc::sync_channel;

    use futures::{channel::mpsc, executor::block_on, SinkExt};
    use tokio::runtime;

    use super::*;

    #[test]
    fn test_last_change_epoch() {
        let f = tempfile::tempfile().unwrap();
        assert!(last_change_epoch(&f.metadata().unwrap()) > 0);
    }

    #[test]
    fn test_extract_thread_name() {
        assert_eq!(&extract_thread_name("test-name-1"), "test-name");
        assert_eq!(&extract_thread_name("grpc-server-5"), "grpc-server");
        assert_eq!(&extract_thread_name("rocksdb:bg1000"), "rocksdb:bg");
        assert_eq!(&extract_thread_name("raftstore-1-100"), "raftstore");
        assert_eq!(&extract_thread_name("snap sender1000"), "snap-sender");
        assert_eq!(&extract_thread_name("snap_sender1000"), "snap-sender");
    }

    // Test there is at most 1 concurrent profiling.
    #[test]
    fn test_profile_guard_concurrency() {
        use std::{thread, time::Duration};

        use futures::{channel::oneshot, TryFutureExt};

        let _test_guard = TEST_PROFILE_MUTEX.lock().unwrap();
        let rt = runtime::Builder::new_multi_thread()
            .worker_threads(4)
            .build()
            .unwrap();

        let expected = "Already in CPU Profiling";

        let (tx1, rx1) = oneshot::channel();
        let rx1 = rx1.map_err(|_| "channel canceled".to_owned());
        let res1 = rt.spawn(start_one_cpu_profile(rx1, 99, false));
        thread::sleep(Duration::from_millis(100));

        let (_tx2, rx2) = oneshot::channel();
        let rx2 = rx2.map_err(|_| "channel canceled".to_owned());
        let res2 = rt.spawn(start_one_cpu_profile(rx2, 99, false));
        assert_eq!(block_on(res2).unwrap().unwrap_err(), expected);

        drop(tx1);
        block_on(res1).unwrap().unwrap_err();

        let expected = "Already in Heap Profiling";

        let (tx1, rx1) = mpsc::channel(1);
        let res1 = rt.spawn(activate_heap_profile(
            Some(rx1),
            std::env::temp_dir(),
            || {},
        ));
        thread::sleep(Duration::from_millis(100));

        let (_tx2, rx2) = mpsc::channel(1);
        let res2 = rt.spawn(activate_heap_profile(
            Some(rx2),
            std::env::temp_dir(),
            || {},
        ));
        assert_eq!(block_on(res2).unwrap().unwrap_err(), expected);

        drop(tx1);
        block_on(res1).unwrap().unwrap();
    }

    #[test]
    fn test_profile_guard_toggle() {
        let _test_guard = TEST_PROFILE_MUTEX.lock().unwrap();
        let rt = runtime::Builder::new_multi_thread()
            .worker_threads(4)
            .build()
            .unwrap();

        // Test activated profiling can be stopped by canceling the period stream.
        let (tx, rx) = mpsc::channel(1);
        let res = rt.spawn(activate_heap_profile(Some(rx), std::env::temp_dir(), || {}));
        drop(tx);
        block_on(res).unwrap().unwrap();

        // Test activated profiling can be stopped by the handle.
        let (tx, rx) = sync_channel::<i32>(1);
        let on_activated = move || drop(tx);
        let check_activated = move || rx.recv().is_err();

        let (_tx, _rx) = mpsc::channel(1);
        let res = rt.spawn(activate_heap_profile(
            Some(_rx),
            std::env::temp_dir(),
            on_activated,
        ));
        assert!(check_activated());
        assert!(deactivate_heap_profile());
        block_on(res).unwrap().unwrap();
    }

    #[test]
    fn test_heap_profile_exit() {
        let _test_guard = TEST_PROFILE_MUTEX.lock().unwrap();
        let rt = runtime::Builder::new_multi_thread()
            .worker_threads(4)
            .build()
            .unwrap();

        // Test heap profiling can be stopped by sending an error.
        let (mut tx, rx) = mpsc::channel(1);
        let res = rt.spawn(activate_heap_profile(Some(rx), std::env::temp_dir(), || {}));
        block_on(tx.send(Err("test".to_string()))).unwrap();
        block_on(res).unwrap().unwrap_err();

        // Test heap profiling can be activated again.
        let (tx, rx) = sync_channel::<i32>(1);
        let on_activated = move || drop(tx);
        let check_activated = move || rx.recv().is_err();

        let (_tx, _rx) = mpsc::channel(1);
        let res = rt.spawn(activate_heap_profile(
            Some(_rx),
            std::env::temp_dir(),
            on_activated,
        ));
        assert!(check_activated());
        assert!(deactivate_heap_profile());
        block_on(res).unwrap().unwrap();
    }
}
