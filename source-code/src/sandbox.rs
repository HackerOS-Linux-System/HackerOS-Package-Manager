use crate::manifest::{Manifest, Sandbox};
use anyhow::{anyhow, Context as _, Result};
use landlock::{
    Access, AccessFs, PathBeneath, PathFd, Ruleset, RulesetAttr, RulesetCreatedAttr, ABI,
};
use nix::mount::{mount, umount2, MntFlags, MsFlags};
use nix::sched::{unshare, CloneFlags};
use nix::sys::stat::{mknod, Mode as MkMode, SFlag, makedev};
use nix::sys::resource::{setrlimit, Resource};
use nix::unistd::{chdir, fork, getpid, pipe, pivot_root, read, write, ForkResult, Gid, Uid, sethostname, execve};
use seccomp::{Action, Context as SeccompContext};
use std::env;
use std::ffi::{CStr, CString};
use std::fs::{create_dir_all, File};
use std::io::Write;
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::process::exit;

/// Główna funkcja do uruchomienia procesu w sandboxie.
pub fn setup_sandbox(
    path: &str,
    manifest: &Manifest,
    is_install: bool,
    bin: Option<&str>,
    extra_args: Vec<String>,
    test: bool,
) -> Result<()> {
    let (read_fd, write_fd) = pipe().context("Pipe creation failed")?;

    match unsafe { fork() }? {
        ForkResult::Parent { child, .. } => {
            let status = nix::sys::wait::waitpid(child, None)?;
            let code = match status {
                nix::sys::wait::WaitStatus::Exited(_, c) => c,
                _ => 1,
            };
            if code != 0 {
                let mut buf = vec![0u8; 4096];
                let n = read(read_fd.as_raw_fd(), &mut buf)?;
                let msg = String::from_utf8_lossy(&buf[0..n]);
                return Err(anyhow!("Sandbox child failed: {}", msg));
            }
            Ok(())
        }
        ForkResult::Child => {
            if let Err(e) = child_setup(path, manifest, is_install, bin, extra_args, test, write_fd.as_raw_fd()) {
                let err_msg = format!("{:?}", e);
                let _ = write(write_fd, err_msg.as_bytes());
                exit(1);
            }
            exit(0);
        }
    }
}

/// Funkcja pomocnicza do wykonywania sekwencji komend w sandboxie (np. build.info)
pub fn run_commands(
    path: &str,
    manifest: &Manifest,
    commands: &[String],
) -> Result<()> {
    let script_path = format!("{}/run_commands.sh", path);
    let script_content = commands.join("\n");
    std::fs::write(&script_path, script_content)?;
    crate::utils::make_executable(Path::new(&script_path))?;
    let result = setup_sandbox(path, manifest, false, Some("run_commands.sh"), vec![], false);
    let _ = std::fs::remove_file(&script_path);
    result
}

/// Funkcja wykonywana w dziecku – właściwa konfiguracja sandboxa.
fn child_setup(
    path: &str,
    manifest: &Manifest,
    is_install: bool,
    bin: Option<&str>,
    extra_args: Vec<String>,
    test: bool,
    _error_fd: i32,
) -> Result<()> {
    let mut flags = CloneFlags::CLONE_NEWUSER
    | CloneFlags::CLONE_NEWNS
    | CloneFlags::CLONE_NEWUTS
    | CloneFlags::CLONE_NEWPID
    | CloneFlags::CLONE_NEWCGROUP;
    if !manifest.sandbox.network {
        flags |= CloneFlags::CLONE_NEWNET;
    }
    if !manifest.sandbox.gui {
        flags |= CloneFlags::CLONE_NEWIPC;
    }
    unshare(flags).context("Unshare failed")?;

    sethostname(&manifest.name)?;

    mount(
        None::<&str>,
        "/",
        None::<&str>,
        MsFlags::MS_PRIVATE | MsFlags::MS_REC,
        None::<&str>,
    )?;

    setup_user_mapping()?;

    let new_root_str = format!("/tmp/hpm_newroot_{}", getpid());
    let new_root = PathBuf::from(&new_root_str);
    create_dir_all(&new_root)?;
    mount(
        Some("tmpfs"),
          new_root_str.as_str(),
          Some("tmpfs"),
          MsFlags::empty(),
          None::<&str>,
    )?;

    let display = env::var("DISPLAY").ok();
    setup_mounts(&new_root, path, &manifest.sandbox, display.as_ref())?;

    pivot_and_chdir(&new_root)?;

    set_resource_limits()?;

    setup_landlock(manifest)?;

    // Uproszczony seccomp – zezwalamy na wszystkie syscalle
    // setup_seccomp()?;

    chdir("/app")?;

    if test {
        return Ok(());
    }

    exec_in_sandbox(is_install, &manifest.install_commands, bin, extra_args)
}

fn setup_user_mapping() -> Result<()> {
    let uid = Uid::current();
    let gid = Gid::current();

    let mut uid_map = File::create("/proc/self/uid_map")?;
    writeln!(uid_map, "0 {} 1", uid)?;

    let mut setgroups = File::create("/proc/self/setgroups")?;
    writeln!(setgroups, "deny")?;

    let mut gid_map = File::create("/proc/self/gid_map")?;
    writeln!(gid_map, "0 {} 1", gid)?;

    Ok(())
}

fn setup_mounts(
    new_root: &Path,
    path: &str,
    sandbox: &Sandbox,
    display: Option<&String>,
) -> Result<()> {
    let ro_paths = vec!["/usr", "/lib", "/lib64", "/bin", "/etc"];
    for p in ro_paths {
        let target = new_root.join(p.trim_start_matches('/'));
        if Path::new(p).exists() {
            create_dir_all(&target)?;
            mount(
                Some(p),
                  target.to_str().unwrap(),
                  None::<&str>,
                  MsFlags::MS_BIND | MsFlags::MS_REC | MsFlags::MS_RDONLY,
                  None::<&str>,
            )?;
        }
    }

    let app_path = new_root.join("app");
    create_dir_all(&app_path)?;
    mount(
        Some(path),
          app_path.to_str().unwrap(),
          None::<&str>,
          MsFlags::MS_BIND | MsFlags::MS_REC,
          None::<&str>,
    )?;

    let tmp_path = new_root.join("tmp");
    create_dir_all(&tmp_path)?;
    mount(
        Some("tmpfs"),
          tmp_path.to_str().unwrap(),
          Some("tmpfs"),
          MsFlags::empty(),
          None::<&str>,
    )?;

    if sandbox.gui || sandbox.full_gui {
        // X11
        if Path::new("/tmp/.X11-unix").exists() {
            let x11_path = new_root.join("tmp/.X11-unix");
            create_dir_all(&x11_path)?;
            mount(
                Some("/tmp/.X11-unix"),
                  x11_path.to_str().unwrap(),
                  None::<&str>,
                  MsFlags::MS_BIND | MsFlags::MS_REC,
                  None::<&str>,
            )?;
        }

        // Wayland
        if let Ok(runtime_dir) = env::var("XDG_RUNTIME_DIR") {
            let wayland_socket = format!("{}/wayland-0", runtime_dir);
            if Path::new(&wayland_socket).exists() {
                let target = new_root.join(runtime_dir.trim_start_matches('/')).join("wayland-0");
                create_dir_all(target.parent().unwrap())?;
                mount(
                    Some(wayland_socket.as_str()),
                      target.to_str().unwrap(),
                      None::<&str>,
                      MsFlags::MS_BIND | MsFlags::MS_REC,
                      None::<&str>,
                )?;
            }
        }

        // D-Bus
        if let Ok(runtime_dir) = env::var("XDG_RUNTIME_DIR") {
            let bus_socket = format!("{}/bus", runtime_dir);
            if Path::new(&bus_socket).exists() {
                let target = new_root.join(runtime_dir.trim_start_matches('/')).join("bus");
                create_dir_all(target.parent().unwrap())?;
                mount(
                    Some(bus_socket.as_str()),
                      target.to_str().unwrap(),
                      None::<&str>,
                      MsFlags::MS_BIND | MsFlags::MS_REC,
                      None::<&str>,
                )?;
            }
        }

        // PulseAudio
        if let Ok(runtime_dir) = env::var("XDG_RUNTIME_DIR") {
            let pulse_dir = format!("{}/pulse", runtime_dir);
            if Path::new(&pulse_dir).exists() {
                let target = new_root.join(runtime_dir.trim_start_matches('/')).join("pulse");
                create_dir_all(target.parent().unwrap())?;
                mount(
                    Some(pulse_dir.as_str()),
                      target.to_str().unwrap(),
                      None::<&str>,
                      MsFlags::MS_BIND | MsFlags::MS_REC,
                      None::<&str>,
                )?;
            }
        }

        // /dev/dri
        if Path::new("/dev/dri").exists() {
            let target = new_root.join("dev/dri");
            create_dir_all(&target)?;
            mount(
                Some("/dev/dri"),
                  target.to_str().unwrap(),
                  None::<&str>,
                  MsFlags::MS_BIND | MsFlags::MS_REC,
                  None::<&str>,
            )?;
        }
    }

    if sandbox.dev {
        let dev_path = new_root.join("dev");
        create_dir_all(&dev_path)?;
        mount(
            Some("tmpfs"),
              dev_path.to_str().unwrap(),
              Some("tmpfs"),
              MsFlags::empty(),
              None::<&str>,
        )?;

        let devices = vec![
            ("null", 1, 3),
            ("zero", 1, 5),
            ("random", 1, 8),
            ("urandom", 1, 9),
            ("tty", 5, 0),
        ];
        for (name, maj, min) in devices {
            let p = dev_path.join(name);
            let _ = mknod(&p, SFlag::S_IFCHR, MkMode::from_bits_truncate(0o666), makedev(maj, min));
        }
    }

    for fs_p in &sandbox.filesystem {
        let target = new_root.join(fs_p.trim_start_matches('/'));
        if let Some(parent) = target.parent() {
            create_dir_all(parent)?;
        }
        if Path::new(fs_p).exists() {
            mount(
                Some(fs_p.as_str()),
                  target.to_str().unwrap(),
                  None::<&str>,
                  MsFlags::MS_BIND | MsFlags::MS_REC,
                  None::<&str>,
            )?;
        }
    }

    let proc_path = new_root.join("proc");
    create_dir_all(&proc_path)?;
    mount(Some("proc"), proc_path.to_str().unwrap(), Some("proc"), MsFlags::empty(), None::<&str>)?;

    let sys_path = new_root.join("sys");
    create_dir_all(&sys_path)?;
    mount(Some("sysfs"), sys_path.to_str().unwrap(), Some("sysfs"), MsFlags::empty(), None::<&str>)?;

    if let Some(d) = display {
        env::set_var("DISPLAY", d);
    }

    Ok(())
}

fn pivot_and_chdir(new_root: &Path) -> Result<()> {
    chdir(new_root)?;
    create_dir_all("old_root")?;
    pivot_root(".", "old_root")?;
    chdir("/")?;
    umount2("/old_root", MntFlags::MNT_DETACH)?;
    Ok(())
}

fn set_resource_limits() -> Result<()> {
    setrlimit(Resource::RLIMIT_CPU, 60, 60)?;
    let mem_limit = 512 * 1024 * 1024;
    setrlimit(Resource::RLIMIT_AS, mem_limit, mem_limit)?;
    setrlimit(Resource::RLIMIT_NPROC, 1024, 1024)?;
    Ok(())
}

fn setup_landlock(manifest: &Manifest) -> Result<()> {
    let abi = ABI::V1;
    let mut ruleset = Ruleset::default()
    .handle_access(AccessFs::from_all(abi))?
    .create()?;

    let ro_access = AccessFs::Execute | AccessFs::ReadFile | AccessFs::ReadDir;

    for path in &["/usr", "/lib", "/lib64", "/bin", "/etc"] {
        if Path::new(path).exists() {
            ruleset = ruleset.add_rule(PathBeneath::new(PathFd::new(path)?, ro_access))?;
        }
    }

    for path in &["/proc", "/sys"] {
        if Path::new(path).exists() {
            ruleset = ruleset.add_rule(PathBeneath::new(PathFd::new(path)?, AccessFs::ReadFile | AccessFs::ReadDir))?;
        }
    }

    ruleset = ruleset.add_rule(PathBeneath::new(PathFd::new("/app")?, AccessFs::from_all(abi)))?;
    ruleset = ruleset.add_rule(PathBeneath::new(PathFd::new("/tmp")?, AccessFs::from_all(abi)))?;

    if manifest.sandbox.dev && Path::new("/dev").exists() {
        ruleset = ruleset.add_rule(PathBeneath::new(PathFd::new("/dev")?, AccessFs::from_all(abi)))?;
    }

    for fs_p in &manifest.sandbox.filesystem {
        if Path::new(fs_p).exists() {
            ruleset = ruleset.add_rule(PathBeneath::new(PathFd::new(fs_p)?, AccessFs::from_all(abi)))?;
        }
    }

    ruleset.restrict_self()?;
    Ok(())
}

fn setup_seccomp() -> Result<()> {
    let ctx = SeccompContext::default(Action::Allow)?;
    ctx.load()?;
    Ok(())
}

fn exec_in_sandbox(
    is_install: bool,
    install_commands: &Vec<String>,
    bin: Option<&str>,
    extra_args: Vec<String>,
) -> Result<()> {
    let (cmd, args_c) = if is_install {
        let install_cmd = if install_commands.is_empty() {
            "echo 'Isolated install complete'".to_string()
        } else {
            install_commands.join(" && ")
        };
        (
            CString::new("/bin/sh")?,
         vec![CString::new("-c")?, CString::new(install_cmd)?],
        )
    } else {
        let bin_path = format!("/app/{}", bin.expect("Bin required"));
        let mut a = vec![CString::new(bin_path.as_str())?];
        for arg in extra_args {
            a.push(CString::new(arg)?);
        }
        (CString::new(bin_path)?, a)
    };

    let args_ptr: Vec<&CStr> = args_c.iter().map(|c| c.as_c_str()).collect();
    execve(&cmd, &args_ptr, &[] as &[&CStr])?;
    unreachable!()
}
