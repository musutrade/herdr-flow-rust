//! Detect allocations after fork without hanging an allocator lock.
pub mod common;
use std::{
    alloc::{GlobalAlloc, Layout, System},
    os::unix::process::CommandExt,
    process::Command,
    sync::atomic::{AtomicBool, Ordering},
};

static ARMED: AtomicBool = AtomicBool::new(false);
static CHILD: AtomicBool = AtomicBool::new(false);
struct AuditAllocator;

fn forbid_child_allocator() {
    if CHILD.load(Ordering::SeqCst) {
        // SAFETY: immediate process termination takes one scalar exit code and
        // does not allocate, lock, unwind or run destructors in the fork child.
        unsafe { libc::_exit(99) };
    }
}

// SAFETY: every allocation operation otherwise delegates unchanged arguments
// to System. The child-only guard terminates instead of touching its allocator.
unsafe impl GlobalAlloc for AuditAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        forbid_child_allocator();
        // SAFETY: the GlobalAlloc caller provides a valid layout.
        unsafe { System.alloc(layout) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        forbid_child_allocator();
        // SAFETY: caller transfers a System-allocated pointer and its layout.
        unsafe { System.dealloc(ptr, layout) }
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        forbid_child_allocator();
        // SAFETY: caller supplies the allocation/layout and valid new size.
        unsafe { System.realloc(ptr, layout, size) }
    }
}
#[global_allocator]
static ALLOCATOR: AuditAllocator = AuditAllocator;

extern "C" fn after_fork() {
    CHILD.store(ARMED.load(Ordering::SeqCst), Ordering::SeqCst);
}

#[test]
fn task_confinement_does_not_allocate_between_fork_and_exec() {
    let root = common::tempdir().unwrap();
    std::env::set_var("HERDR_TASK_ENV_ROOT", root.path().join("state"));
    let mut parent = root.path().to_path_buf();
    for _ in 0..10 {
        parent.push("x".repeat(100));
    }
    common::create_dir_all(&parent).unwrap();
    let environment = herdr_flow::task_env::create_environment(
        "prefork-probe",
        &parent.join("work"),
        vec![],
        &parent.join("temp"),
    )
    .unwrap();
    // SAFETY: static C callback uses only lock-free atomics; no pointers are
    // retained. This integration binary has one test and owns every child.
    assert_eq!(
        unsafe { libc::pthread_atfork(None, None, Some(after_fork)) },
        0
    );
    let mut control = Command::new("/bin/true");
    // SAFETY: callback checks one atomic and, on failure, exits via a syscall.
    unsafe {
        control.pre_exec(|| {
            if !CHILD.load(Ordering::SeqCst) {
                libc::_exit(98);
            }
            Ok(())
        });
    }
    ARMED.store(true, Ordering::SeqCst);
    let control_result = control.status();
    ARMED.store(false, Ordering::SeqCst);
    assert!(
        control_result.unwrap().success(),
        "allocator probe control failed"
    );
    let fd_count = || std::fs::read_dir("/proc/self/fd").unwrap().count();
    let before = fd_count();
    let parent_status = std::fs::read_to_string("/proc/self/status").unwrap();
    let no_new_privs = |s: &str| {
        s.lines()
            .find(|line| line.starts_with("NoNewPrivs:"))
            .unwrap()
            .to_owned()
    };
    let mut command = environment.command("/bin/true");
    assert_eq!(fd_count(), before + 1, "one prepared ruleset owner");
    assert_eq!(
        no_new_privs(&parent_status),
        no_new_privs(&std::fs::read_to_string("/proc/self/status").unwrap())
    );
    ARMED.store(true, Ordering::SeqCst);
    let result = command.status();
    let reused = command.status();
    ARMED.store(false, Ordering::SeqCst);
    drop(command);
    assert_eq!(fd_count(), before, "dropping Command releases the ruleset");
    assert!(reused.unwrap().success(), "prepared command is reusable");
    // A vanished root fails preparation after ruleset creation. Drop must close
    // that partial owner, and spawn must report errno without allocating.
    let saved_root = parent.join("saved-work");
    std::fs::rename(&environment.workspace_root, &saved_root).unwrap();
    let mut invalid = environment.command("/bin/true");
    assert_eq!(
        fd_count(),
        before,
        "failed preparation releases the ruleset"
    );
    ARMED.store(true, Ordering::SeqCst);
    let failed = invalid.status();
    ARMED.store(false, Ordering::SeqCst);
    assert_eq!(failed.unwrap_err().raw_os_error(), Some(libc::ENOENT));
    std::fs::rename(&saved_root, &environment.workspace_root).unwrap();
    herdr_flow::task_env::destroy_environment(&environment).unwrap();
    assert!(
        result.unwrap().success(),
        "confinement touched the allocator after fork (exit 99)"
    );
}
