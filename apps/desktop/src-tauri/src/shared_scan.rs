//! One physical discovery at a time; consumers share recent immutable inventories.
use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Condvar, Mutex,
    },
    time::{Duration, Instant},
};
const TTL: Duration = Duration::from_secs(15 * 60);
pub(crate) trait Inventory: Send + Sync {
    fn roots(&self) -> &[PathBuf];
    fn fresh(&self) -> bool;
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{sync::mpsc, thread};
    struct Data {
        roots: Vec<PathBuf>,
        valid: Arc<AtomicBool>,
    }
    impl Inventory for Data {
        fn roots(&self) -> &[PathBuf] {
            &self.roots
        }
        fn fresh(&self) -> bool {
            self.valid.load(Ordering::Acquire)
        }
    }
    fn roots(s: &[&str]) -> Vec<PathBuf> {
        s.iter().map(PathBuf::from).collect()
    }
    fn data(paths: &[PathBuf], seed: Option<Arc<Data>>) -> Data {
        let mut roots = seed.map_or(vec![], |v| v.roots.clone());
        roots.extend_from_slice(paths);
        Data {
            roots,
            valid: Arc::new(AtomicBool::new(true)),
        }
    }
    fn cancel() -> Arc<AtomicBool> {
        Arc::new(AtomicBool::new(false))
    }
    fn wait_for(mut predicate: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(3);
        while !predicate() {
            assert!(Instant::now() < deadline, "timed out");
            thread::sleep(Duration::from_millis(1));
        }
    }
    #[test]
    fn completed_scope_reuses_and_superset_builds_only_missing_roots() {
        let c = Coordinator::default();
        let all = roots(&["C", "D", "E"]);
        let first = c
            .ensure(
                &all[..1],
                false,
                cancel(),
                |_| {},
                |r, s, _, _| Ok(data(r, s)),
            )
            .unwrap();
        let home = c
            .ensure(
                &all[..1],
                false,
                cancel(),
                |_| {},
                |_, _, _, _| panic!("unexpected traversal"),
            )
            .unwrap();
        assert!(home.reused);
        assert!(Arc::ptr_eq(&first.data, &home.data));
        let next = c
            .ensure(
                &all,
                false,
                cancel(),
                |_| {},
                |r, s, _, _| {
                    assert_eq!(r, &all[1..]);
                    Ok(data(r, s))
                },
            )
            .unwrap();
        assert_eq!(next.data.roots, all);
        assert_eq!(c.runs(), 2);
    }
    #[test]
    fn simultaneous_consumers_and_force_join_one_physical_job() {
        let c = Coordinator::default();
        let paths = roots(&["C"]);
        let (tx, rx) = mpsc::channel();
        let (release, go) = mpsc::channel();
        let go = Mutex::new(go);
        thread::scope(|scope| {
            let owner = scope.spawn(|| {
                c.ensure(
                    &paths,
                    false,
                    cancel(),
                    |_| {},
                    |r, s, _, report| {
                        report(SharedProgress::default());
                        tx.send(()).unwrap();
                        go.lock().unwrap().recv().unwrap();
                        Ok(data(r, s))
                    },
                )
                .unwrap()
            });
            rx.recv().unwrap();
            let waiter = scope.spawn(|| {
                c.ensure(
                    &paths,
                    true,
                    cancel(),
                    |_| {},
                    |_, _, _, _| panic!("duplicate traversal"),
                )
                .unwrap()
            });
            wait_for(|| c.inner.lock().unwrap().watchers.len() == 2);
            // Give the waiting consumer a chance to accept the active scope.
            wait_for(|| c.inner.lock().unwrap().next == 2);
            release.send(()).unwrap();
            let a = owner.join().unwrap();
            let b = waiter.join().unwrap();
            assert!(Arc::ptr_eq(&a.data, &b.data));
            assert_eq!(c.runs(), 1);
        });
    }
    #[test]
    fn explicit_refresh_and_invalidation_remove_old_authority() {
        let c = Coordinator::default();
        let paths = roots(&["C"]);
        let first = c
            .ensure(&paths, false, cancel(), |_| {}, |r, s, _, _| Ok(data(r, s)))
            .unwrap();
        let second = c
            .ensure(
                &paths,
                true,
                cancel(),
                |_| {},
                |r, s, _, _| {
                    assert!(s.is_none());
                    Ok(data(r, s))
                },
            )
            .unwrap();
        assert!(second.epoch > first.epoch);
        assert!(!Arc::ptr_eq(&first.data, &second.data));
        c.invalidate();
        assert!(c.peek().is_none());
        assert!(c.epoch() > second.epoch);
    }
    #[test]
    fn expired_inventory_is_not_used_or_renewed_by_a_new_scope() {
        let c = Coordinator::default();
        let paths = roots(&["C", "D"]);
        let first = c
            .ensure(
                &paths[..1],
                false,
                cancel(),
                |_| {},
                |r, s, _, _| Ok(data(r, s)),
            )
            .unwrap();
        first.data.valid.store(false, Ordering::Release);
        assert!(c.peek().is_none());
        c.ensure(
            &paths,
            false,
            cancel(),
            |_| {},
            |r, s, _, _| {
                assert!(s.is_none());
                assert_eq!(r, paths);
                Ok(data(r, s))
            },
        )
        .unwrap();
        assert_eq!(c.runs(), 2);
    }
    #[test]
    fn cancel_one_consumer_keeps_job_needed_by_another() {
        let c = Coordinator::default();
        let paths = roots(&["C"]);
        let owner_cancel = cancel();
        let (tx, rx) = mpsc::channel();
        let (release, go) = mpsc::channel();
        let go = Mutex::new(go);
        thread::scope(|scope| {
            let owner = scope.spawn(|| {
                c.ensure(
                    &paths,
                    false,
                    owner_cancel.clone(),
                    |_| {},
                    |r, s, physical, report| {
                        tx.send(()).unwrap();
                        go.lock().unwrap().recv().unwrap();
                        report(SharedProgress::default());
                        assert!(!physical.load(Ordering::Acquire));
                        Ok(data(r, s))
                    },
                )
            });
            rx.recv().unwrap();
            let other = scope.spawn(|| {
                c.ensure(
                    &paths,
                    false,
                    cancel(),
                    |_| {},
                    |_, _, _, _| panic!("duplicate traversal"),
                )
            });
            wait_for(|| c.inner.lock().unwrap().watchers.len() == 2);
            owner_cancel.store(true, Ordering::Release);
            c.cancel_changed();
            release.send(()).unwrap();
            assert!(owner.join().unwrap().is_err());
            assert!(other.join().unwrap().is_ok());
            assert_eq!(c.runs(), 1);
        });
    }
    #[test]
    fn last_cancel_and_invalidation_during_build_discard_result() {
        for invalidate in [false, true] {
            let c = Coordinator::default();
            let paths = roots(&["C"]);
            let token = cancel();
            let result = c.ensure(
                &paths,
                false,
                token.clone(),
                |_| {},
                |r, s, physical, report| {
                    if invalidate {
                        c.invalidate();
                    } else {
                        token.store(true, Ordering::Release);
                    }
                    report(SharedProgress::default());
                    assert!(physical.load(Ordering::Acquire));
                    Ok(data(r, s))
                },
            );
            assert!(result.is_err());
            assert!(c.peek().is_none());
        }
    }
    #[test]
    fn panic_releases_active_slot_and_allows_retry() {
        let c = Coordinator::<Data>::default();
        let paths = roots(&["C"]);
        assert!(c
            .ensure(
                &paths,
                false,
                cancel(),
                |_| {},
                |_, _, _, _| panic!("fixture failure")
            )
            .is_err());
        assert!(c.inner.lock().unwrap().active.is_none());
        assert!(c
            .ensure(&paths, false, cancel(), |_| {}, |r, s, _, _| Ok(data(r, s)))
            .is_ok());
    }
}
#[derive(Clone, Default)]
pub(crate) struct SharedProgress {
    pub phase: &'static str,
    pub root: String,
    pub files: u64,
    pub bytes: u64,
    pub directories: u64,
    pub denied: u64,
    pub protected: u64,
    pub linked: u64,
}
struct Watcher {
    roots: Vec<PathBuf>,
    cancel: Arc<AtomicBool>,
}
struct Active {
    epoch: u64,
    roots: Vec<PathBuf>,
    cancel: Arc<AtomicBool>,
}
struct Inner<T> {
    ready: Option<Arc<T>>,
    active: Option<Active>,
    watchers: HashMap<u64, Watcher>,
    next: u64,
    epoch: u64,
    progress: SharedProgress,
    physical_runs: u64,
}
impl<T> Default for Inner<T> {
    fn default() -> Self {
        Self {
            ready: None,
            active: None,
            watchers: HashMap::new(),
            next: 0,
            epoch: 0,
            progress: SharedProgress::default(),
            physical_runs: 0,
        }
    }
}
pub(crate) struct Coordinator<T> {
    inner: Mutex<Inner<T>>,
    changed: Condvar,
}
impl<T> Default for Coordinator<T> {
    fn default() -> Self {
        Self {
            inner: Mutex::new(Inner::default()),
            changed: Condvar::new(),
        }
    }
}
pub(crate) struct ResultView<T> {
    pub data: Arc<T>,
    pub epoch: u64,
    pub reused: bool,
}
struct WatchGuard<'a, T> {
    coordinator: &'a Coordinator<T>,
    id: u64,
}
impl<T> Drop for WatchGuard<'_, T> {
    fn drop(&mut self) {
        if let Ok(mut s) = self.coordinator.inner.lock() {
            s.watchers.remove(&self.id);
            reconcile(&mut s);
            self.coordinator.changed.notify_all();
        }
    }
}
fn reconcile<T>(s: &mut Inner<T>) {
    if let Some(active) = &s.active {
        let interested = s.watchers.values().any(|w| {
            !w.cancel.load(Ordering::Acquire) && w.roots.iter().any(|r| active.roots.contains(r))
        });
        if !interested {
            active.cancel.store(true, Ordering::Release)
        }
    }
}
impl<T: Inventory> Coordinator<T> {
    pub fn peek(&self) -> Option<ResultView<T>> {
        let s = self.inner.lock().ok()?;
        let data = s.ready.as_ref().filter(|v| v.fresh())?.clone();
        Some(ResultView {
            data,
            epoch: s.epoch,
            reused: true,
        })
    }
    pub fn epoch(&self) -> u64 {
        self.inner.lock().map_or(u64::MAX, |s| s.epoch)
    }
    pub fn invalidate(&self) {
        if let Ok(mut s) = self.inner.lock() {
            s.epoch += 1;
            s.ready = None;
            if let Some(a) = &s.active {
                a.cancel.store(true, Ordering::Release)
            }
            self.changed.notify_all();
        }
    }
    pub fn cancel_changed(&self) {
        if let Ok(mut s) = self.inner.lock() {
            reconcile(&mut s);
            self.changed.notify_all();
        }
    }
    pub fn runs(&self) -> u64 {
        self.inner.lock().map_or(0, |s| s.physical_runs)
    }
    pub fn ensure(
        &self,
        roots: &[PathBuf],
        force: bool,
        cancel: Arc<AtomicBool>,
        mut on_progress: impl FnMut(SharedProgress),
        mut build: impl FnMut(
            &[PathBuf],
            Option<Arc<T>>,
            &AtomicBool,
            &mut dyn FnMut(SharedProgress),
        ) -> Result<T, String>,
    ) -> Result<ResultView<T>, String> {
        let mut s = self.inner.lock().map_err(|e| e.to_string())?;
        let force = force
            && !s
                .active
                .as_ref()
                .is_some_and(|a| roots.iter().all(|r| a.roots.contains(r)));
        s.next += 1;
        let id = s.next;
        s.watchers.insert(
            id,
            Watcher {
                roots: roots.into(),
                cancel: cancel.clone(),
            },
        );
        drop(s);
        let _watch = WatchGuard {
            coordinator: self,
            id,
        };
        let started = Instant::now();
        let mut force = force;
        loop {
            if cancel.load(Ordering::Acquire) {
                return Err("检查已取消".into());
            }
            if started.elapsed() > TTL + TTL {
                return Err("等待共享检查超时，请重试".into());
            }
            let mut s = self.inner.lock().map_err(|e| e.to_string())?;
            if s.ready.as_ref().is_some_and(|r| !r.fresh()) {
                s.ready = None;
            }
            if !force {
                if let Some(ready) = s
                    .ready
                    .as_ref()
                    .filter(|r| roots.iter().all(|root| r.roots().contains(root)))
                {
                    return Ok(ResultView {
                        data: ready.clone(),
                        epoch: s.epoch,
                        reused: true,
                    });
                }
            }
            if s.active
                .as_ref()
                .is_some_and(|a| roots.iter().all(|r| a.roots.contains(r)))
            {
                force = false;
            }
            if s.active.is_some() {
                let p = s.progress.clone();
                let (guard, _) = self
                    .changed
                    .wait_timeout(s, Duration::from_millis(250))
                    .map_err(|e| e.to_string())?;
                drop(guard);
                on_progress(p);
                continue;
            }
            let seed = if force { None } else { s.ready.clone() };
            let missing = roots
                .iter()
                .filter(|r| seed.as_ref().is_none_or(|seed| !seed.roots().contains(r)))
                .cloned()
                .collect::<Vec<_>>();
            if force {
                s.epoch += 1;
                s.ready = None;
            }
            let epoch = s.epoch;
            let physical_cancel = Arc::new(AtomicBool::new(false));
            s.physical_runs += 1;
            s.active = Some(Active {
                epoch,
                roots: missing.clone(),
                cancel: physical_cancel.clone(),
            });
            s.progress = SharedProgress::default();
            drop(s);
            let mut report = |p: SharedProgress| {
                if let Ok(mut s) = self.inner.lock() {
                    s.progress = p.clone();
                    reconcile(&mut s);
                    self.changed.notify_all();
                }
                if !cancel.load(Ordering::Acquire) {
                    on_progress(p)
                }
            };
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                build(&missing, seed, &physical_cancel, &mut report)
            }))
            .unwrap_or_else(|_| Err("共享检查任务异常，请重试".into()));
            let mut s = self.inner.lock().map_err(|e| e.to_string())?;
            let still_current =
                s.epoch == epoch && s.active.as_ref().is_some_and(|a| a.epoch == epoch);
            s.active = None;
            let result = match result {
                Ok(value)
                    if still_current
                        && !physical_cancel.load(Ordering::Acquire)
                        && value.fresh() =>
                {
                    let value = Arc::new(value);
                    s.ready = Some(value.clone());
                    Ok(ResultView {
                        data: value,
                        epoch,
                        reused: false,
                    })
                }
                Ok(_) => Err("检查已取消、结果已过期或文件发生变化，请重新检查".into()),
                Err(e) => Err(e),
            };
            self.changed.notify_all();
            drop(s);
            if cancel.load(Ordering::Acquire) {
                return Err("检查已取消".into());
            }
            return result;
        }
    }
}
