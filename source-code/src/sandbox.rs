use crate::manifest::{Manifest, Sandbox};
use miette::{Result, bail, miette, IntoDiagnostic};
use landlock::{
    Access, AccessFs, PathBeneath, PathFd, Ruleset, RulesetAttr, RulesetCreatedAttr, ABI,
};
use nix::mount::{mount, umount2, MntFlags, MsFlags};
use nix::sched::{unshare, CloneFlags};
use nix::sys::stat::{mknod, Mode as MkMode, SFlag, makedev};
use nix::sys::resource::{setrlimit, Resource};
use nix::unistd::{
    chdir, fork, getpid, pipe, pivot_root, read, write,
    ForkResult, Gid, Uid, sethostname, execve,
};
use seccomp::{Action, Context as SeccompContext, Rule, Compare, Op};
use std::env;
use std::ffi::{CStr, CString};
use std::fs::{create_dir_all, File};
use std::io::Write;
use std::os::fd::OwnedFd;          // ← correct public path
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::process::exit;

// ---------------------------------------------------------------------------
// Sandbox mode
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum SandboxMode {
    Full,
    Compat,
    None,
}

// ---------------------------------------------------------------------------
// Resource limits
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy)]
pub struct ResourceLimits {
    pub cpu_secs:  u64,
    pub mem_bytes: u64,
    pub nproc:     u64,
}

impl ResourceLimits {
    pub fn for_run() -> Self {
        Self { cpu_secs: 0, mem_bytes: 4 * 1024 * 1024 * 1024, nproc: 2048 }
    }
    pub fn for_build() -> Self {
        Self { cpu_secs: 0, mem_bytes: 8 * 1024 * 1024 * 1024, nproc: 8192 }
    }
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

pub fn setup_sandbox(
    path: &str,
    manifest: &Manifest,
    is_install: bool,
    bin: Option<&str>,
    extra_args: Vec<String>,
    test: bool,
) -> Result<()> {
    let mode = pick_mode(manifest);
    match mode {
        SandboxMode::None   => exec_direct(manifest, is_install, bin, extra_args),
        SandboxMode::Compat => run_compat(path, manifest, is_install, bin, extra_args, test),
        SandboxMode::Full   => run_full(path, manifest, is_install, bin, extra_args, test, ResourceLimits::for_run()),
    }
}

pub fn run_commands(path: &str, manifest: &Manifest, commands: &[String]) -> Result<()> {
    let script_path = format!("{}/run_commands.sh", path);
    std::fs::write(&script_path, format!("#!/bin/sh\nset -e\n{}", commands.join("\n")))
    .into_diagnostic()?;
    crate::utils::make_executable(Path::new(&script_path))?;
    let result = run_compat(path, manifest, false, Some("run_commands.sh"), vec![], false);
    let _ = std::fs::remove_file(&script_path);
    result
}

// ---------------------------------------------------------------------------
// Mode selection
// ---------------------------------------------------------------------------

fn pick_mode(manifest: &Manifest) -> SandboxMode {
    if manifest.sandbox_disabled {
        return SandboxMode::None;
    }
    let s = &manifest.sandbox;
    if s.gui || s.full_gui || s.network || !s.filesystem.is_empty() {
        return SandboxMode::Compat;
    }
    SandboxMode::Full
}

// ---------------------------------------------------------------------------
// Direct exec (no sandbox)
// ---------------------------------------------------------------------------

fn exec_direct(
    manifest: &Manifest,
    is_install: bool,
    bin: Option<&str>,
    extra_args: Vec<String>,
) -> Result<()> {
    let (read_fd, write_fd) = pipe().into_diagnostic()?;
    match unsafe { fork() }.into_diagnostic()? {
        ForkResult::Parent { child, .. } => wait_child(child, read_fd),
        ForkResult::Child => {
            if let Err(e) = exec_in_sandbox(is_install, &manifest.install_commands, bin, extra_args) {
                let msg = format!("{:?}", e);
                let _ = write(write_fd, msg.as_bytes());
                exit(1);
            }
            exit(0);
        }
    }
}

// ---------------------------------------------------------------------------
// Compat mode — mount namespace only
// ---------------------------------------------------------------------------

fn run_compat(
    path: &str,
    manifest: &Manifest,
    is_install: bool,
    bin: Option<&str>,
    extra_args: Vec<String>,
    test: bool,
) -> Result<()> {
    let (read_fd, write_fd) = pipe().into_diagnostic()?;
    match unsafe { fork() }.into_diagnostic()? {
        ForkResult::Parent { child, .. } => wait_child(child, read_fd),
        ForkResult::Child => {
            if let Err(e) = compat_setup(path, manifest, is_install, bin, extra_args, test) {
                let msg = format!("{:?}", e);
                let _ = write(write_fd, msg.as_bytes());
                exit(1);
            }
            exit(0);
        }
    }
}

fn compat_setup(
    path: &str,
    manifest: &Manifest,
    is_install: bool,
    bin: Option<&str>,
    extra_args: Vec<String>,
    test: bool,
) -> Result<()> {
    unshare(CloneFlags::CLONE_NEWNS).into_diagnostic()?;
    mount(None::<&str>, "/", None::<&str>, MsFlags::MS_PRIVATE | MsFlags::MS_REC, None::<&str>)
    .into_diagnostic()?;

    apply_resource_limits(ResourceLimits::for_run())?;
    setup_seccomp()?;

    if test { return Ok(()); }
    exec_from_path(path, is_install, &manifest.install_commands, bin, extra_args)
}

// ---------------------------------------------------------------------------
// Full mode
// ---------------------------------------------------------------------------

fn run_full(
    path: &str,
    manifest: &Manifest,
    is_install: bool,
    bin: Option<&str>,
    extra_args: Vec<String>,
    test: bool,
    limits: ResourceLimits,
) -> Result<()> {
    let (read_fd, write_fd) = pipe().into_diagnostic()?;
    match unsafe { fork() }.into_diagnostic()? {
        ForkResult::Parent { child, .. } => wait_child(child, read_fd),
        ForkResult::Child => {
            if let Err(e) = full_setup(path, manifest, is_install, bin, extra_args, test, limits) {
                let msg = format!("{:?}", e);
                let _ = write(write_fd, msg.as_bytes());
                exit(1);
            }
            exit(0);
        }
    }
}

fn full_setup(
    path: &str,
    manifest: &Manifest,
    is_install: bool,
    bin: Option<&str>,
    extra_args: Vec<String>,
    test: bool,
    limits: ResourceLimits,
) -> Result<()> {
    let mut flags = CloneFlags::CLONE_NEWUSER
    | CloneFlags::CLONE_NEWNS
    | CloneFlags::CLONE_NEWUTS
    | CloneFlags::CLONE_NEWPID
    | CloneFlags::CLONE_NEWCGROUP;
    if !manifest.sandbox.network { flags |= CloneFlags::CLONE_NEWNET; }
    if !manifest.sandbox.gui    { flags |= CloneFlags::CLONE_NEWIPC; }
    unshare(flags).into_diagnostic()?;
    sethostname(&manifest.name).into_diagnostic()?;

    mount(None::<&str>, "/", None::<&str>, MsFlags::MS_PRIVATE | MsFlags::MS_REC, None::<&str>)
    .into_diagnostic()?;

    setup_user_mapping()?;

    let new_root_str = format!("/tmp/hpm_newroot_{}", getpid());
    let new_root = PathBuf::from(&new_root_str);
    create_dir_all(&new_root).into_diagnostic()?;
    mount(Some("tmpfs"), new_root_str.as_str(), Some("tmpfs"), MsFlags::empty(), None::<&str>)
    .into_diagnostic()?;

    let display = env::var("DISPLAY").ok();
    setup_mounts(&new_root, path, &manifest.sandbox, display.as_ref())?;
    pivot_and_chdir(&new_root)?;

    apply_resource_limits(limits)?;
    setup_landlock(manifest)?;
    setup_seccomp()?;

    chdir("/app").into_diagnostic()?;
    if test { return Ok(()); }
    exec_in_sandbox(is_install, &manifest.install_commands, bin, extra_args)
}

// ---------------------------------------------------------------------------
// Wait helper — uses std::os::fd::OwnedFd (public)
// ---------------------------------------------------------------------------

fn wait_child(child: nix::unistd::Pid, read_fd: OwnedFd) -> Result<()> {
    let status = nix::sys::wait::waitpid(child, None).into_diagnostic()?;
    let code = match status {
        nix::sys::wait::WaitStatus::Exited(_, c) => c,
        _ => 1,
    };
    if code != 0 {
        let mut buf = vec![0u8; 4096];
        let n = read(read_fd.as_raw_fd(), &mut buf).unwrap_or(0);
        let msg = String::from_utf8_lossy(&buf[0..n]);
        if msg.is_empty() {
            bail!("Process exited with code {}", code);
        }
        bail!("Sandbox error: {}", msg.trim());
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// User mapping
// ---------------------------------------------------------------------------

fn setup_user_mapping() -> Result<()> {
    let uid = Uid::current();
    let gid = Gid::current();
    let mut uid_map = File::create("/proc/self/uid_map").into_diagnostic()?;
    writeln!(uid_map, "0 {} 1", uid).into_diagnostic()?;
    let mut setgroups = File::create("/proc/self/setgroups").into_diagnostic()?;
    writeln!(setgroups, "deny").into_diagnostic()?;
    let mut gid_map = File::create("/proc/self/gid_map").into_diagnostic()?;
    writeln!(gid_map, "0 {} 1", gid).into_diagnostic()?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Mounts (full mode)
// ---------------------------------------------------------------------------

fn setup_mounts(new_root: &Path, path: &str, sandbox: &Sandbox, display: Option<&String>) -> Result<()> {
    for p in &["/usr", "/lib", "/lib64", "/lib32", "/bin", "/sbin", "/etc"] {
        if !Path::new(p).exists() { continue; }
        let target = new_root.join(p.trim_start_matches('/'));
        create_dir_all(&target).into_diagnostic()?;
        mount(Some(*p), target.to_str().unwrap(), None::<&str>,
              MsFlags::MS_BIND | MsFlags::MS_REC | MsFlags::MS_RDONLY, None::<&str>)
        .into_diagnostic()?;
    }

    let app_path = new_root.join("app");
    create_dir_all(&app_path).into_diagnostic()?;
    mount(Some(path), app_path.to_str().unwrap(), None::<&str>,
          MsFlags::MS_BIND | MsFlags::MS_REC, None::<&str>).into_diagnostic()?;

          let tmp_path = new_root.join("tmp");
          create_dir_all(&tmp_path).into_diagnostic()?;
          mount(Some("tmpfs"), tmp_path.to_str().unwrap(), Some("tmpfs"), MsFlags::empty(), None::<&str>)
          .into_diagnostic()?;

          if let Ok(home) = env::var("HOME") {
              if Path::new(&home).exists() {
                  let target = new_root.join(home.trim_start_matches('/'));
                  create_dir_all(&target).into_diagnostic()?;
                  mount(Some(home.as_str()), target.to_str().unwrap(), None::<&str>,
                        MsFlags::MS_BIND | MsFlags::MS_REC, None::<&str>).into_diagnostic()?;
              }
          }

          if sandbox.gui || sandbox.full_gui {
              bind_gui_sockets(new_root)?;
          }

          if sandbox.dev || sandbox.gui || sandbox.full_gui {
              setup_dev(new_root, sandbox.full_gui)?;
          } else {
              let dev = new_root.join("dev");
              create_dir_all(&dev).into_diagnostic()?;
              mount(Some("tmpfs"), dev.to_str().unwrap(), Some("tmpfs"), MsFlags::empty(), None::<&str>)
              .into_diagnostic()?;
              for (name, maj, min) in &[("null",1u64,3u64),("zero",1,5),("random",1,8),("urandom",1,9),("tty",5,0)] {
                  let _ = mknod(&dev.join(name), SFlag::S_IFCHR, MkMode::from_bits_truncate(0o666), makedev(*maj, *min));
              }
          }

          for fs_p in &sandbox.filesystem {
              if !Path::new(fs_p).exists() { continue; }
              let target = new_root.join(fs_p.trim_start_matches('/'));
              if let Some(parent) = target.parent() { create_dir_all(parent).into_diagnostic()?; }
              mount(Some(fs_p.as_str()), target.to_str().unwrap(), None::<&str>,
                    MsFlags::MS_BIND | MsFlags::MS_REC, None::<&str>).into_diagnostic()?;
          }

          let proc_path = new_root.join("proc");
          create_dir_all(&proc_path).into_diagnostic()?;
          mount(Some("proc"), proc_path.to_str().unwrap(), Some("proc"), MsFlags::empty(), None::<&str>)
          .into_diagnostic()?;

          let sys_path = new_root.join("sys");
          create_dir_all(&sys_path).into_diagnostic()?;
          mount(Some("sysfs"), sys_path.to_str().unwrap(), Some("sysfs"), MsFlags::empty(), None::<&str>)
          .into_diagnostic()?;

          if let Some(d) = display { env::set_var("DISPLAY", d); }
          Ok(())
}

fn bind_gui_sockets(new_root: &Path) -> Result<()> {
    if Path::new("/tmp/.X11-unix").exists() {
        let x11 = new_root.join("tmp/.X11-unix");
        create_dir_all(&x11).into_diagnostic()?;
        mount(Some("/tmp/.X11-unix"), x11.to_str().unwrap(), None::<&str>,
              MsFlags::MS_BIND | MsFlags::MS_REC, None::<&str>).into_diagnostic()?;
    }
    if let Ok(runtime_dir) = env::var("XDG_RUNTIME_DIR") {
        bind_socket_if_exists(&format!("{}/wayland-0", runtime_dir), new_root)?;
        bind_socket_if_exists(&format!("{}/bus", runtime_dir), new_root)?;
        bind_socket_if_exists(&format!("{}/pipewire-0", runtime_dir), new_root)?;
        bind_dir_if_exists(&format!("{}/pulse", runtime_dir), new_root)?;
    }
    if Path::new("/dev/dri").exists() {
        let dri = new_root.join("dev/dri");
        create_dir_all(&dri).into_diagnostic()?;
        mount(Some("/dev/dri"), dri.to_str().unwrap(), None::<&str>,
              MsFlags::MS_BIND | MsFlags::MS_REC, None::<&str>).into_diagnostic()?;
    }
    Ok(())
}

fn bind_socket_if_exists(src: &str, new_root: &Path) -> Result<()> {
    if !Path::new(src).exists() { return Ok(()); }
    let target = new_root.join(src.trim_start_matches('/'));
    if let Some(parent) = target.parent() { create_dir_all(parent).into_diagnostic()?; }
    File::create(&target).into_diagnostic()?;
    mount(Some(src), target.to_str().unwrap(), None::<&str>,
          MsFlags::MS_BIND | MsFlags::MS_REC, None::<&str>).into_diagnostic()?;
          Ok(())
}

fn bind_dir_if_exists(src: &str, new_root: &Path) -> Result<()> {
    if !Path::new(src).exists() { return Ok(()); }
    let target = new_root.join(src.trim_start_matches('/'));
    create_dir_all(&target).into_diagnostic()?;
    mount(Some(src), target.to_str().unwrap(), None::<&str>,
          MsFlags::MS_BIND | MsFlags::MS_REC, None::<&str>).into_diagnostic()?;
          Ok(())
}

fn setup_dev(new_root: &Path, full_gui: bool) -> Result<()> {
    let dev_path = new_root.join("dev");
    create_dir_all(&dev_path).into_diagnostic()?;
    mount(Some("tmpfs"), dev_path.to_str().unwrap(), Some("tmpfs"), MsFlags::empty(), None::<&str>)
    .into_diagnostic()?;
    for (name, maj, min) in &[
        ("null",1u64,3u64),("zero",1,5),("random",1,8),("urandom",1,9),
        ("tty",5,0),("ptmx",5,2),("fuse",10,229),
    ] {
        let _ = mknod(&dev_path.join(name), SFlag::S_IFCHR,
                      MkMode::from_bits_truncate(0o666), makedev(*maj, *min));
    }
    if full_gui {
        let shm = dev_path.join("shm");
        create_dir_all(&shm).into_diagnostic()?;
        mount(Some("tmpfs"), shm.to_str().unwrap(), Some("tmpfs"), MsFlags::empty(), None::<&str>)
        .into_diagnostic()?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// pivot_root
// ---------------------------------------------------------------------------

fn pivot_and_chdir(new_root: &Path) -> Result<()> {
    chdir(new_root).into_diagnostic()?;
    create_dir_all("old_root").into_diagnostic()?;
    pivot_root(".", "old_root").into_diagnostic()?;
    chdir("/").into_diagnostic()?;
    umount2("/old_root", MntFlags::MNT_DETACH).into_diagnostic()?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Resource limits
// ---------------------------------------------------------------------------

fn apply_resource_limits(limits: ResourceLimits) -> Result<()> {
    if limits.cpu_secs > 0 {
        setrlimit(Resource::RLIMIT_CPU, limits.cpu_secs, limits.cpu_secs).into_diagnostic()?;
    }
    if limits.mem_bytes > 0 {
        setrlimit(Resource::RLIMIT_AS, limits.mem_bytes, limits.mem_bytes).into_diagnostic()?;
    }
    setrlimit(Resource::RLIMIT_NPROC, limits.nproc, limits.nproc).into_diagnostic()?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Landlock
// ---------------------------------------------------------------------------

fn setup_landlock(manifest: &Manifest) -> Result<()> {
    let abi = ABI::V1;
    let mut ruleset = Ruleset::default()
    .handle_access(AccessFs::from_all(abi))
    .map_err(|e| miette!("Landlock ruleset: {}", e))?
    .create()
    .map_err(|e| miette!("Landlock create: {}", e))?;

    let ro = AccessFs::Execute | AccessFs::ReadFile | AccessFs::ReadDir;
    let rw = AccessFs::from_all(abi);

    for path in &["/usr", "/lib", "/lib64", "/lib32", "/bin", "/sbin", "/etc"] {
        if !Path::new(path).exists() { continue; }
        ruleset = ruleset.add_rule(
            PathBeneath::new(PathFd::new(path).map_err(|e| miette!("{}", e))?, ro)
        ).map_err(|e| miette!("Landlock: {}", e))?;
    }
    for path in &["/proc", "/sys"] {
        if !Path::new(path).exists() { continue; }
        ruleset = ruleset.add_rule(
            PathBeneath::new(PathFd::new(path).map_err(|e| miette!("{}", e))?,
                             AccessFs::ReadFile | AccessFs::ReadDir)
        ).map_err(|e| miette!("Landlock: {}", e))?;
    }
    for path in &["/app", "/tmp"] {
        if !Path::new(path).exists() { continue; }
        ruleset = ruleset.add_rule(
            PathBeneath::new(PathFd::new(path).map_err(|e| miette!("{}", e))?, rw)
        ).map_err(|e| miette!("Landlock: {}", e))?;
    }
    if let Ok(home) = env::var("HOME") {
        if Path::new(&home).exists() {
            ruleset = ruleset.add_rule(
                PathBeneath::new(PathFd::new(&home).map_err(|e| miette!("{}", e))?, rw)
            ).map_err(|e| miette!("Landlock: {}", e))?;
        }
    }
    if manifest.sandbox.dev && Path::new("/dev").exists() {
        ruleset = ruleset.add_rule(
            PathBeneath::new(PathFd::new("/dev").map_err(|e| miette!("{}", e))?, rw)
        ).map_err(|e| miette!("Landlock: {}", e))?;
    }
    for fs_p in &manifest.sandbox.filesystem {
        if !Path::new(fs_p).exists() { continue; }
        ruleset = ruleset.add_rule(
            PathBeneath::new(PathFd::new(fs_p).map_err(|e| miette!("{}", e))?, rw)
        ).map_err(|e| miette!("Landlock: {}", e))?;
    }
    ruleset.restrict_self().map_err(|e| miette!("Landlock restrict_self: {}", e))?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Seccomp
//
// seccomp 0.1.2: Compare::arg(n).using(Op).with(val).build() -> Option<Cmp>
// None means the build failed (invalid args) — we use unwrap_or_else to bail.
// Default Allow + explicit deny for dangerous syscalls only.
// ---------------------------------------------------------------------------

fn make_cmp() -> Result<seccomp::Cmp> {
    // arg0 >= 0 — always true, used as required-but-ignored comparator
    Compare::arg(0)
    .using(Op::Ge)
    .with(0)
    .build()
    .ok_or_else(|| miette!("Failed to build seccomp comparator"))
}

fn deny_syscall(ctx: &mut SeccompContext, nr: i64) -> Result<()> {
    let cmp = make_cmp()?;
    let rule = Rule::new(nr as usize, cmp, Action::Errno(libc::EPERM));
    ctx.add_rule(rule).map_err(|e| miette!("seccomp add_rule: {}", e))
}

fn setup_seccomp() -> Result<()> {
    let mut ctx = SeccompContext::default(Action::Allow)
    .map_err(|e| miette!("Seccomp context: {}", e))?;

    for &nr in &[
        libc::SYS_kexec_load,
        libc::SYS_kexec_file_load,
        libc::SYS_init_module,
        libc::SYS_finit_module,
        libc::SYS_delete_module,
        libc::SYS_ptrace,
        libc::SYS_process_vm_readv,
        libc::SYS_process_vm_writev,
        libc::SYS_iopl,
        libc::SYS_ioperm,
        libc::SYS_perf_event_open,
        libc::SYS_syslog,
        libc::SYS_acct,
        libc::SYS_swapon,
        libc::SYS_swapoff,
        libc::SYS_reboot,
        libc::SYS_keyctl,
        libc::SYS_add_key,
        libc::SYS_request_key,
        libc::SYS_bpf,
        libc::SYS_userfaultfd,
    ] {
        deny_syscall(&mut ctx, nr)?;
    }

    ctx.load().map_err(|e| miette!("Seccomp load: {}", e))?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Exec helpers
// ---------------------------------------------------------------------------

fn exec_in_sandbox(
    is_install: bool,
    install_commands: &[String],
    bin: Option<&str>,
    extra_args: Vec<String>,
) -> Result<()> {
    let (cmd_str, args_strs) = if is_install {
        let install_cmd = if install_commands.is_empty() {
            "echo 'Install complete'".to_string()
        } else {
            install_commands.join(" && ")
        };
        ("/bin/sh".to_string(), vec!["/bin/sh".to_string(), "-c".to_string(), install_cmd])
    } else {
        let bin_path = format!("/app/{}", bin.expect("bin required"));
        let mut args = vec![bin_path.clone()];
        args.extend(extra_args);
        (bin_path, args)
    };
    do_execve(&cmd_str, &args_strs)
}

fn exec_from_path(
    path: &str,
    is_install: bool,
    install_commands: &[String],
    bin: Option<&str>,
    extra_args: Vec<String>,
) -> Result<()> {
    let (cmd_str, args_strs) = if is_install {
        let install_cmd = if install_commands.is_empty() {
            "echo 'Install complete'".to_string()
        } else {
            install_commands.join(" && ")
        };
        ("/bin/sh".to_string(), vec!["/bin/sh".to_string(), "-c".to_string(), install_cmd])
    } else {
        let bin_path = format!("{}/{}", path, bin.expect("bin required"));
        let mut args = vec![bin_path.clone()];
        args.extend(extra_args);
        (bin_path, args)
    };
    do_execve(&cmd_str, &args_strs)
}

fn do_execve(cmd: &str, args: &[String]) -> Result<()> {
    let cmd_c = CString::new(cmd).map_err(|e| miette!("{}", e))?;
    let args_c: Vec<CString> = args.iter()
    .map(|a| CString::new(a.as_str()).map_err(|e| miette!("{}", e)))
    .collect::<Result<Vec<_>>>()?;
    let args_ptr: Vec<&CStr> = args_c.iter().map(|c| c.as_c_str()).collect();
    execve(&cmd_c, &args_ptr, &[] as &[&CStr])
    .map_err(|e| miette!("execve '{}': {}", cmd, e))?;
    unreachable!()
}
