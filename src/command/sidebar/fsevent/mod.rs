//! Watcher implementation for Darwin's FSEvents API
//!
//! The FSEvents API provides a mechanism to notify clients about directories they ought to re-scan
//! in order to keep their internal data structures up-to-date with respect to the true state of
//! the file system. (For example, when files or directories are created, modified, or removed.) It
//! sends these notifications "in bulk", possibly notifying the client of changes to several
//! directories in a single callback.
//!
//! For more information see the [FSEvents API reference][ref].
//!
//! TODO: document event translation
//!
//! [ref]: https://developer.apple.com/library/mac/documentation/Darwin/Reference/FSEvents_Ref/

// Keep the upstream backend structure for comparison with notify 9.0.0-rc.3.
#![allow(non_upper_case_globals, dead_code)]
#![allow(clippy::unnecessary_cast, clippy::collapsible_if)]

use notify::{Config, Error, EventHandler, EventKindMask, RecursiveMode, Result, Watcher};
use notify::{PathOp, event::*};
use objc2_core_foundation as cf;
use objc2_core_services as fs;
use std::collections::HashMap;
use std::ffi::{CStr, OsStr};
use std::fmt;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::ptr::{self, NonNull};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Sender, channel as unbounded};
use std::sync::{Arc, Mutex};
use std::thread;

bitflags::bitflags! {
  #[repr(C)]
  #[derive(Debug)]
  struct StreamFlags: u32 {
    const NONE = fs::kFSEventStreamEventFlagNone;
    const MUST_SCAN_SUBDIRS = fs::kFSEventStreamEventFlagMustScanSubDirs;
    const USER_DROPPED = fs::kFSEventStreamEventFlagUserDropped;
    const KERNEL_DROPPED = fs::kFSEventStreamEventFlagKernelDropped;
    const IDS_WRAPPED = fs::kFSEventStreamEventFlagEventIdsWrapped;
    const HISTORY_DONE = fs::kFSEventStreamEventFlagHistoryDone;
    const ROOT_CHANGED = fs::kFSEventStreamEventFlagRootChanged;
    const MOUNT = fs::kFSEventStreamEventFlagMount;
    const UNMOUNT = fs::kFSEventStreamEventFlagUnmount;
    const ITEM_CREATED = fs::kFSEventStreamEventFlagItemCreated;
    const ITEM_REMOVED = fs::kFSEventStreamEventFlagItemRemoved;
    const INODE_META_MOD = fs::kFSEventStreamEventFlagItemInodeMetaMod;
    const ITEM_RENAMED = fs::kFSEventStreamEventFlagItemRenamed;
    const ITEM_MODIFIED = fs::kFSEventStreamEventFlagItemModified;
    const FINDER_INFO_MOD = fs::kFSEventStreamEventFlagItemFinderInfoMod;
    const ITEM_CHANGE_OWNER = fs::kFSEventStreamEventFlagItemChangeOwner;
    const ITEM_XATTR_MOD = fs::kFSEventStreamEventFlagItemXattrMod;
    const IS_FILE = fs::kFSEventStreamEventFlagItemIsFile;
    const IS_DIR = fs::kFSEventStreamEventFlagItemIsDir;
    const IS_SYMLINK = fs::kFSEventStreamEventFlagItemIsSymlink;
    const OWN_EVENT = fs::kFSEventStreamEventFlagOwnEvent;
    const IS_HARDLINK = fs::kFSEventStreamEventFlagItemIsHardlink;
    const IS_LAST_HARDLINK = fs::kFSEventStreamEventFlagItemIsLastHardlink;
    const ITEM_CLONED = fs::kFSEventStreamEventFlagItemCloned;
  }
}

const RUNLOOP_STOP_RECHECK_SECONDS: cf::CFTimeInterval = 0.5;

/// FSEvents-based `Watcher` implementation
pub struct FsEventWatcher {
    paths: cf::CFRetained<cf::CFMutableArray<cf::CFString>>,
    since_when: fs::FSEventStreamEventId,
    latency: cf::CFTimeInterval,
    flags: fs::FSEventStreamCreateFlags,
    event_handler: Arc<Mutex<dyn EventHandler>>,
    runloop: Option<RunLoopHandle>,
    recursive_info: HashMap<PathBuf, bool>,
    event_kinds: EventKindMask,
}

#[derive(Debug)]
struct RunLoopHandle {
    runloop: cf::CFRetained<cf::CFRunLoop>,
    stop_flag: Arc<AtomicBool>,
    thread_handle: thread::JoinHandle<()>,
}

impl fmt::Debug for FsEventWatcher {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("FsEventWatcher")
            .field("paths", &self.paths)
            .field("since_when", &self.since_when)
            .field("latency", &self.latency)
            .field("flags", &self.flags)
            .field("event_handler", &Arc::as_ptr(&self.event_handler))
            .field("runloop", &self.runloop)
            .field("recursive_info", &self.recursive_info)
            .finish()
    }
}

// FsEventWatcher is not Send/Sync automatically.
// It's Send because the pointer is not used in other threads.
unsafe impl Send for FsEventWatcher {}

// It's Sync because all methods that change the mutable state use `&mut self`.
unsafe impl Sync for FsEventWatcher {}

fn translate_flags(flags: StreamFlags, precise: bool) -> Vec<Event> {
    let mut evs = Vec::new();

    // «Denotes a sentinel event sent to mark the end of the "historical" events
    // sent as a result of specifying a `sinceWhen` value in the FSEvents.Create
    // call that created this event stream. After invoking the client's callback
    // with all the "historical" events that occurred before now, the client's
    // callback will be invoked with an event where the HistoryDone flag is set.
    // The client should ignore the path supplied in this callback.»
    // — https://www.mbsplugins.eu/FSEventsNextEvent.shtml
    //
    // As a result, we just stop processing here and return an empty vec, which
    // will ignore this completely and not emit any Events whatsoever.
    if flags.contains(StreamFlags::HISTORY_DONE) {
        return evs;
    }

    // FSEvents provides two possible hints as to why events were dropped,
    // however documentation on what those mean is scant, so we just pass them
    // through in the info attr field. The intent is clear enough, and the
    // additional information is provided if the user wants it.
    if flags.contains(StreamFlags::MUST_SCAN_SUBDIRS) {
        let e = Event::new(EventKind::Other).set_flag(Flag::Rescan);
        evs.push(if flags.contains(StreamFlags::USER_DROPPED) {
            e.set_info("rescan: user dropped")
        } else if flags.contains(StreamFlags::KERNEL_DROPPED) {
            e.set_info("rescan: kernel dropped")
        } else {
            e
        });
    }

    // In imprecise mode, let's not even bother parsing the kind of the event
    // except for the above very special events.
    if !precise {
        evs.push(Event::new(EventKind::Any));
        return evs;
    }

    // A watched root changed (renamed or removed). If the flags provide a hint,
    // prefer that over guessing. Otherwise, treat it as a removal to avoid
    // misclassifying a delete as a rename.
    let root_changed = flags.contains(StreamFlags::ROOT_CHANGED);
    if root_changed {
        let kind = if flags.contains(StreamFlags::ITEM_REMOVED) {
            if flags.contains(StreamFlags::IS_DIR) {
                EventKind::Remove(RemoveKind::Folder)
            } else if flags.contains(StreamFlags::IS_FILE) {
                EventKind::Remove(RemoveKind::File)
            } else {
                EventKind::Remove(RemoveKind::Any)
            }
        } else if flags.contains(StreamFlags::ITEM_RENAMED) {
            EventKind::Modify(ModifyKind::Name(RenameMode::From))
        } else {
            EventKind::Remove(RemoveKind::Any)
        };

        evs.push(Event::new(kind).set_info("root changed"));
    }

    // A path was mounted at the event path; we treat that as a create.
    if flags.contains(StreamFlags::MOUNT) {
        evs.push(Event::new(EventKind::Create(CreateKind::Other)).set_info("mount"));
    }

    // A path was unmounted at the event path; we treat that as a remove.
    if flags.contains(StreamFlags::UNMOUNT) {
        evs.push(Event::new(EventKind::Remove(RemoveKind::Other)).set_info("mount"));
    }

    if flags.contains(StreamFlags::ITEM_CREATED) {
        evs.push(if flags.contains(StreamFlags::IS_DIR) {
            Event::new(EventKind::Create(CreateKind::Folder))
        } else if flags.contains(StreamFlags::IS_FILE) {
            Event::new(EventKind::Create(CreateKind::File))
        } else {
            let e = Event::new(EventKind::Create(CreateKind::Other));
            if flags.contains(StreamFlags::IS_SYMLINK) {
                e.set_info("is: symlink")
            } else if flags.contains(StreamFlags::IS_HARDLINK) {
                e.set_info("is: hardlink")
            } else if flags.contains(StreamFlags::ITEM_CLONED) {
                e.set_info("is: clone")
            } else {
                Event::new(EventKind::Create(CreateKind::Any))
            }
        });
    }

    // FSEvents provides no mechanism to associate the old and new sides of a
    // rename event.
    // Avoid emitting duplicate events around a root change by checking `root_changed`.
    if flags.contains(StreamFlags::ITEM_RENAMED) && !root_changed {
        evs.push(Event::new(EventKind::Modify(ModifyKind::Name(
            RenameMode::Any,
        ))));
    }

    // This is only described as "metadata changed", but it may be that it's
    // only emitted for some more precise subset of events... if so, will need
    // amending, but for now we have an Any-shaped bucket to put it in.
    if flags.contains(StreamFlags::INODE_META_MOD) {
        evs.push(Event::new(EventKind::Modify(ModifyKind::Metadata(
            MetadataKind::Any,
        ))));
    }

    if flags.contains(StreamFlags::FINDER_INFO_MOD) {
        evs.push(
            Event::new(EventKind::Modify(ModifyKind::Metadata(MetadataKind::Other)))
                .set_info("meta: finder info"),
        );
    }

    if flags.contains(StreamFlags::ITEM_CHANGE_OWNER) {
        evs.push(Event::new(EventKind::Modify(ModifyKind::Metadata(
            MetadataKind::Ownership,
        ))));
    }

    if flags.contains(StreamFlags::ITEM_XATTR_MOD) {
        evs.push(Event::new(EventKind::Modify(ModifyKind::Metadata(
            MetadataKind::Extended,
        ))));
    }

    // This is specifically described as a data change, which we take to mean
    // is a content change.
    if flags.contains(StreamFlags::ITEM_MODIFIED) {
        evs.push(Event::new(EventKind::Modify(ModifyKind::Data(
            DataChange::Content,
        ))));
    }

    // Avoid emitting duplicate events around a root change by checking `root_changed`.
    if flags.contains(StreamFlags::ITEM_REMOVED) && !root_changed {
        evs.push(if flags.contains(StreamFlags::IS_DIR) {
            Event::new(EventKind::Remove(RemoveKind::Folder))
        } else if flags.contains(StreamFlags::IS_FILE) {
            Event::new(EventKind::Remove(RemoveKind::File))
        } else {
            let e = Event::new(EventKind::Remove(RemoveKind::Other));
            if flags.contains(StreamFlags::IS_SYMLINK) {
                e.set_info("is: symlink")
            } else if flags.contains(StreamFlags::IS_HARDLINK) {
                e.set_info("is: hardlink")
            } else if flags.contains(StreamFlags::ITEM_CLONED) {
                e.set_info("is: clone")
            } else {
                Event::new(EventKind::Remove(RemoveKind::Any))
            }
        });
    }

    // `ITEM_CLONED` can be present alongside other flags (including create/modify/remove).
    // Preserve any existing `info` (like "root changed"), but annotate otherwise so downstream
    // can detect and filter clone-related events. See https://github.com/notify-rs/notify/issues/465.
    if flags.contains(StreamFlags::ITEM_CLONED) {
        for ev in &mut evs {
            if ev.info().is_none() {
                ev.attrs.set_info("is: clone");
            }
        }
    }

    if flags.contains(StreamFlags::OWN_EVENT) {
        for ev in &mut evs {
            *ev = std::mem::take(ev).set_process_id(std::process::id());
        }
    }

    evs
}

struct StreamContextInfo {
    event_handler: Arc<Mutex<dyn EventHandler>>,
    recursive_info: HashMap<PathBuf, bool>,
    event_kinds: EventKindMask,
}

// Free the context when the stream created by `FSEventStreamCreate` is released.
unsafe extern "C-unwind" fn release_context(info: *const libc::c_void) {
    // Safety:
    // - The [documentation] for `FSEventStreamContext` states that `release` is only
    //   called when the stream is deallocated, so it is safe to convert `info` back into a
    //   box and drop it.
    //
    // [docs]: https://developer.apple.com/documentation/coreservices/fseventstreamcontext?language=objc
    unsafe {
        drop(Box::from_raw(
            info as *const StreamContextInfo as *mut StreamContextInfo,
        ));
    }
}

impl FsEventWatcher {
    fn from_event_handler(
        event_handler: Arc<Mutex<dyn EventHandler>>,
        event_kinds: EventKindMask,
    ) -> Result<Self> {
        Ok(FsEventWatcher {
            paths: cf::CFMutableArray::empty(),
            since_when: fs::kFSEventStreamEventIdSinceNow,
            latency: 0.0,
            flags: fs::kFSEventStreamCreateFlagFileEvents
                | fs::kFSEventStreamCreateFlagNoDefer
                | fs::kFSEventStreamCreateFlagWatchRoot,
            event_handler,
            runloop: None,
            recursive_info: HashMap::new(),
            event_kinds,
        })
    }

    fn watch_inner(&mut self, path: &Path, recursive_mode: RecursiveMode) -> Result<()> {
        self.stop();
        let result = self.append_path(path, recursive_mode);
        self.run()?;
        result
    }

    fn unwatch_inner(&mut self, path: &Path) -> Result<()> {
        self.stop();
        let result = self.remove_path(path);
        self.run()?;
        result
    }

    fn update_paths_inner(
        &mut self,
        ops: Vec<notify::PathOp>,
    ) -> std::result::Result<(), notify::UpdatePathsError> {
        self.stop();

        let result = update_paths(ops, |op| match op {
            notify::PathOp::Watch(path, config) => self
                .append_path(&path, config.recursive_mode())
                .map_err(|e| (PathOp::Watch(path, config), e)),
            notify::PathOp::Unwatch(path) => self
                .remove_path(&path)
                .map_err(|e| (PathOp::Unwatch(path), e)),
        });

        match self.run() {
            Err(run_error) => match result {
                Ok(()) => Err(notify::UpdatePathsError {
                    source: run_error,
                    origin: None,
                    remaining: Default::default(),
                }),
                Err(path_op_error) => {
                    log::error!(
                        "Unable to run fsevents watcher after updating paths error: {run_error:?}"
                    );
                    Err(path_op_error)
                }
            },
            Ok(()) => result,
        }
    }

    #[inline]
    fn is_running(&self) -> bool {
        self.runloop.is_some()
    }

    fn stop(&mut self) {
        if !self.is_running() {
            return;
        }

        if let Some(RunLoopHandle {
            runloop,
            stop_flag,
            thread_handle,
        }) = self.runloop.take()
        {
            // Don't wait for the runloop to become "waiting" before stopping; if the
            // stream is under heavy load that can delay shutdown indefinitely.
            stop_flag.store(true, Ordering::Release);
            runloop.stop();
            runloop.wake_up();
            // Wait for the thread to shut down.
            thread_handle.join().expect("thread to shut down");
        }
    }

    fn remove_path(&mut self, path: &Path) -> Result<()> {
        let mut err: *mut cf::CFError = ptr::null_mut();
        let Some(cf_path) = (unsafe { path_to_cfstring_ref(path, &mut err) }) else {
            if let Some(err) = NonNull::new(err) {
                let _ = unsafe { cf::CFRetained::from_raw(err) };
            }
            return Err(Error::watch_not_found().add_path(path.into()));
        };

        let mut to_remove = Vec::new();
        for (idx, item) in self.paths.iter().enumerate() {
            if item.compare(
                Some(&cf_path),
                cf::CFStringCompareFlags::CompareCaseInsensitive,
            ) == cf::CFComparisonResult::CompareEqualTo
            {
                to_remove.push(idx as cf::CFIndex);
            }
        }

        for idx in to_remove.iter().rev() {
            unsafe {
                cf::CFMutableArray::remove_value_at_index(Some(self.paths.as_opaque()), *idx)
            };
        }

        let p = if let Ok(canonicalized_path) = path.canonicalize() {
            canonicalized_path
        } else {
            path.to_owned()
        };
        match self.recursive_info.remove(&p) {
            Some(_) => Ok(()),
            None => Err(Error::watch_not_found()),
        }
    }

    // https://github.com/thibaudgg/rb-fsevent/blob/master/ext/fsevent_watch/main.c
    fn append_path(&mut self, path: &Path, recursive_mode: RecursiveMode) -> Result<()> {
        if !path.exists() {
            return Err(Error::path_not_found().add_path(path.into()));
        }
        let canonical_path = path.to_path_buf().canonicalize()?;
        let mut err: *mut cf::CFError = ptr::null_mut();
        let Some(cf_path) = (unsafe { path_to_cfstring_ref(path, &mut err) }) else {
            if let Some(err) = NonNull::new(err) {
                let _ = unsafe { cf::CFRetained::from_raw(err) };
            }
            // Most likely the directory was deleted, or permissions changed,
            // while the above code was running.
            return Err(Error::path_not_found().add_path(path.into()));
        };
        self.paths.append(&cf_path);

        self.recursive_info.insert(
            canonical_path,
            matches!(recursive_mode, RecursiveMode::Recursive),
        );
        Ok(())
    }

    fn run(&mut self) -> Result<()> {
        if self.paths.is_empty() {
            return Ok(());
        }

        // We need to associate the stream context with our callback in order to propagate events
        // to the rest of the system. This will be owned by the stream, and will be freed when the
        // stream is closed. This means we will leak the context if we panic before reaching
        // `FSEventStreamRelease`.
        let context = Box::into_raw(Box::new(StreamContextInfo {
            event_handler: self.event_handler.clone(),
            recursive_info: self.recursive_info.clone(),
            event_kinds: self.event_kinds,
        }));

        let stream_context = fs::FSEventStreamContext {
            version: 0,
            info: context as *mut libc::c_void,
            retain: None,
            release: Some(release_context),
            copyDescription: None,
        };

        let stream = unsafe {
            fs::FSEventStreamCreate(
                cf::kCFAllocatorDefault,
                Some(callback),
                &stream_context as *const _ as *mut _,
                self.paths.as_opaque(),
                self.since_when,
                self.latency,
                self.flags,
            )
        };

        // Wrapper to help send CFRunLoop types across threads.
        struct CFRunLoopSendWrapper(cf::CFRetained<cf::CFRunLoop>);

        // Safety:
        // - According to the Apple documentation, it's safe to move `CFRunLoop`s across threads.
        //   https://developer.apple.com/library/archive/documentation/Cocoa/Conceptual/Multithreading/ThreadSafetySummary/ThreadSafetySummary.html
        unsafe impl Send for CFRunLoopSendWrapper {}

        // Wrapper to help send FSEventStreamRef types across threads.
        struct FSEventStreamSendWrapper(fs::FSEventStreamRef);

        // TODO: Write docs for the safety of this impl.
        // SAFETY: Unclear?
        unsafe impl Send for FSEventStreamSendWrapper {}

        // move into thread
        let stream = FSEventStreamSendWrapper(stream);

        // channel to pass runloop around
        let (rl_tx, rl_rx) = unbounded();

        // Used to stop the runloop thread without relying on privileged APIs or
        // on `CFRunLoopIsWaiting()` becoming true under heavy event load.
        let stop_flag = Arc::new(AtomicBool::new(false));
        let stop_flag_thread = Arc::clone(&stop_flag);

        let thread_handle = thread::Builder::new()
            .name("notify-rs fsevents loop".to_string())
            .spawn(move || {
                let _ = &stream;
                let stream = stream.0;

                unsafe {
                    // Safety:
                    // This may panic if OOM occurs.
                    // Related: https://github.com/madsmtm/objc2/issues/797
                    let cur_runloop =
                        cf::CFRunLoop::current().expect("Failed to get current runloop");

                    #[allow(deprecated)]
                    fs::FSEventStreamScheduleWithRunLoop(
                        stream,
                        &cur_runloop,
                        cf::kCFRunLoopDefaultMode.expect("Failed to get default runloop mode"),
                    );
                    if !fs::FSEventStreamStart(stream) {
                        fs::FSEventStreamInvalidate(stream);
                        fs::FSEventStreamRelease(stream);
                        rl_tx
                            .send(Err(Error::generic("unable to start FSEvent stream")))
                            .expect("Unable to send error for FSEventStreamStart");
                        return;
                    }

                    // `stop()` will call `CFRunLoopStop` + `CFRunLoopWakeUp` and then join this
                    // thread.
                    rl_tx
                        .send(Ok(CFRunLoopSendWrapper(cur_runloop)))
                        .expect("Unable to send runloop to watcher");

                    // `CFRunLoopStop` is discarded if `stop()` calls it before this thread
                    // enters the runloop, so re-check the stop flag after each bounded run.
                    while !stop_flag_thread.load(Ordering::Acquire) {
                        let result = cf::CFRunLoop::run_in_mode(
                            cf::kCFRunLoopDefaultMode,
                            RUNLOOP_STOP_RECHECK_SECONDS,
                            false,
                        );
                        if result == cf::CFRunLoopRunResult::Finished {
                            break;
                        }
                    }
                    fs::FSEventStreamStop(stream);
                    fs::FSEventStreamInvalidate(stream);
                    fs::FSEventStreamRelease(stream);
                }
            })?;
        // block until runloop has been sent
        let runloop_wrapper = match rl_rx.recv() {
            Ok(Ok(runloop_wrapper)) => runloop_wrapper,
            Ok(Err(err)) => {
                thread_handle
                    .join()
                    .expect("thread to shut down after FSEvent stream startup failure");
                return Err(err);
            }
            Err(_) => {
                thread_handle
                    .join()
                    .expect("thread to shut down after FSEvent stream startup channel close");
                return Err(Error::generic(
                    "unable to receive FSEvent stream startup result",
                ));
            }
        };
        self.runloop = Some(RunLoopHandle {
            runloop: runloop_wrapper.0,
            stop_flag,
            thread_handle,
        });

        Ok(())
    }

    fn configure_raw_mode(&mut self, _config: Config, tx: Sender<Result<bool>>) {
        tx.send(Ok(false))
            .expect("configuration channel disconnect");
    }
}

unsafe extern "C-unwind" fn callback(
    stream_ref: fs::ConstFSEventStreamRef,
    info: *mut libc::c_void,
    num_events: libc::size_t,                          // size_t numEvents
    event_paths: NonNull<libc::c_void>,                // void *eventPaths
    event_flags: NonNull<fs::FSEventStreamEventFlags>, // const FSEventStreamEventFlags eventFlags[]
    event_ids: NonNull<fs::FSEventStreamEventId>,      // const FSEventStreamEventId eventIds[]
) {
    // Never unwind into CoreServices; if something goes wrong, drop the events and log.
    // This also protects against panics from user-provided `EventHandler` implementations.
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| unsafe {
        callback_impl(
            stream_ref,
            info,
            num_events,
            event_paths,
            event_flags,
            event_ids,
        )
    }))
    .map_err(|_| {
        log::error!("panic in FSEvents callback; dropping pending events");
    });
}

unsafe fn callback_impl(
    _stream_ref: fs::ConstFSEventStreamRef,
    info: *mut libc::c_void,
    num_events: libc::size_t,                          // size_t numEvents
    event_paths: NonNull<libc::c_void>,                // void *eventPaths
    event_flags: NonNull<fs::FSEventStreamEventFlags>, // const FSEventStreamEventFlags eventFlags[]
    _event_ids: NonNull<fs::FSEventStreamEventId>,     // const FSEventStreamEventId eventIds[]
) {
    let event_paths = event_paths.as_ptr() as *const *const libc::c_char;
    let info = info as *const StreamContextInfo;
    let event_handler = unsafe { &(*info).event_handler };

    for p in 0..num_events {
        // Paths are not guaranteed to be valid UTF-8 (e.g. NFS); keep them as raw bytes.
        let path = unsafe { CStr::from_ptr(*event_paths.add(p)) };
        let path = PathBuf::from(OsStr::from_bytes(path.to_bytes()));

        let raw_flag = unsafe { *event_flags.as_ptr().add(p) } as u32;
        let flag = StreamFlags::from_bits_truncate(raw_flag);
        let unknown_bits = raw_flag & !StreamFlags::all().bits();
        if unknown_bits != 0 {
            // `FSEventStreamEventFlags` is an extensible bitfield; tolerate future flags.
            log::trace!("unknown FSEventStreamEventFlags bits: 0x{unknown_bits:08x}");
        }

        let mut handle_event = false;
        for (p, r) in unsafe { &(*info).recursive_info } {
            if path.starts_with(p) {
                if *r || &path == p {
                    handle_event = true;
                    break;
                } else if let Some(parent_path) = path.parent() {
                    if parent_path == p {
                        handle_event = true;
                        break;
                    }
                }
            }
        }

        if !handle_event {
            continue;
        }

        log::trace!("FSEvent: path = `{}`, flag = {:?}", path.display(), flag);

        for ev in translate_flags(flag, true).into_iter() {
            // TODO: precise
            let ev = ev.add_path(path.clone());
            // Filter events based on EventKindMask
            if !unsafe { &(*info).event_kinds }.matches(&ev.kind) {
                continue; // Skip events that don't match the mask
            }
            let mut event_handler = match event_handler.lock() {
                Ok(guard) => guard,
                Err(poisoned) => poisoned.into_inner(),
            };
            // Protect against panicking event handlers, which would otherwise unwind into
            // the CoreServices callback.
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                event_handler.handle_event(Ok(ev));
            }))
            .map_err(|_| {
                log::error!("panic in FSEvents event handler; dropping event");
            });
        }
    }
}

impl Watcher for FsEventWatcher {
    /// Create a new watcher.
    fn new<F: EventHandler>(event_handler: F, config: Config) -> Result<Self> {
        Self::from_event_handler(Arc::new(Mutex::new(event_handler)), config.event_kinds())
    }

    fn watch(&mut self, path: &Path, recursive_mode: RecursiveMode) -> Result<()> {
        self.watch_inner(path, recursive_mode)
    }

    fn unwatch(&mut self, path: &Path) -> Result<()> {
        self.unwatch_inner(path)
    }

    fn update_paths(
        &mut self,
        ops: Vec<PathOp>,
    ) -> std::result::Result<(), notify::UpdatePathsError> {
        self.update_paths_inner(ops)
    }

    fn configure(&mut self, config: Config) -> Result<bool> {
        let (tx, rx) = unbounded();
        self.configure_raw_mode(config, tx);
        rx.recv()?
    }

    fn watched_paths(&self) -> Result<Vec<(PathBuf, RecursiveMode)>> {
        // Unlike the channel-based backends, FSEvents keeps watch state on the watcher itself.
        // The runloop callback gets a cloned snapshot in `StreamContextInfo`, so it does not
        // mutate or read this map concurrently.
        Ok(self
            .recursive_info
            .iter()
            .map(|(path, is_recursive)| {
                (
                    path.clone(),
                    if *is_recursive {
                        RecursiveMode::Recursive
                    } else {
                        RecursiveMode::NonRecursive
                    },
                )
            })
            .collect())
    }

    fn kind() -> notify::WatcherKind {
        notify::WatcherKind::Fsevent
    }
}

impl Drop for FsEventWatcher {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Grabbed from <https://docs.rs/fsevent-sys/4.1.0/src/fsevent_sys/core_foundation.rs.html#149-230>.
///
/// TODO: Could we simplify this?
unsafe fn path_to_cfstring_ref(
    source: &Path,
    err: &mut *mut cf::CFError,
) -> Option<cf::CFRetained<cf::CFString>> {
    let url = cf::CFURL::from_file_path(source)?;

    let mut placeholder = url.absolute_url()?;

    let imaginary = cf::CFMutableArray::empty();

    while !unsafe { placeholder.resource_is_reachable(err) } {
        if let Some(child) = placeholder.last_path_component() {
            imaginary.insert(0, &*child);
        }

        placeholder = cf::CFURL::new_copy_deleting_last_path_component(None, Some(&placeholder))?;
    }

    let url = unsafe { cf::CFURL::new_file_reference_url(None, Some(&placeholder), err) }?;

    let mut placeholder = unsafe { cf::CFURL::new_file_path_url(None, Some(&url), err) }?;

    for component in imaginary {
        placeholder = cf::CFURL::new_copy_appending_path_component(
            None,
            Some(&placeholder),
            Some(&component),
            false,
        )?;
    }

    placeholder.file_system_path(cf::CFURLPathStyle::CFURLPOSIXPathStyle)
}

fn update_paths<F>(
    ops: Vec<PathOp>,
    mut apply: F,
) -> std::result::Result<(), notify::UpdatePathsError>
where
    F: FnMut(PathOp) -> std::result::Result<(), (PathOp, Error)>,
{
    let mut iter = ops.into_iter();
    while let Some(op) = iter.next() {
        if let Err((error_op, source)) = apply(op) {
            return Err(notify::UpdatePathsError {
                source,
                origin: Some(error_op),
                remaining: iter.collect(),
            });
        }
    }
    Ok(())
}
