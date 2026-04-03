use crate::manifest::{Manifest, Sandbox};
use miette::{Result, bail, miette, IntoDiagnostic};
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
    let (read_fd, write_fd) = pipe().into_diagnostic()?;

    match unsafe { fork() }.into_diagnostic()? {
        ForkResult::Parent { child, .. } => {
            let status = nix::sys::wait::waitpid(child, None).into_diagnostic()?;
            let code = match status {
                nix::sys::wait::WaitStatus::Exited(_, c) => c,
                _ => 1,
            };
            if code != 0 {
                let mut buf = vec![0u8; 4096];
                let n = read(read_fd.as_raw_fd(), &mut buf).into_diagnostic()?;
                let msg = String::from_utf8_lossy(&buf[0..n]);
                bail!("Sandbox child failed: {}", msg);
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
    std::fs::write(&script_path, script_content).into_diagnostic()?;
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
    unshare(flags).into_diagnostic()?;

    sethostname(&manifest.name).into_diagnostic()?;

    mount(
        None::<&str>,
        "/",
        None::<&str>,
        MsFlags::MS_PRIVATE | MsFlags::MS_REC,
        None::<&str>,
    ).into_diagnostic()?;

    setup_user_mapping()?;

    let new_root_str = format!("/tmp/hpm_newroot_{}", getpid());
    let new_root = PathBuf::from(&new_root_str);
    create_dir_all(&new_root).into_diagnostic()?;
    mount(
        Some("tmpfs"),
          new_root_str.as_str(),
          Some("tmpfs"),
          MsFlags::empty(),
          None::<&str>,
    ).into_diagnostic()?;

    let display = env::var("DISPLAY").ok();
    setup_mounts(&new_root, path, &manifest.sandbox, display.as_ref())?;

    pivot_and_chdir(&new_root)?;

    set_resource_limits()?;

    setup_landlock(manifest)?;

    // Uproszczony seccomp – zezwalamy na wszystkie syscalle
    // setup_seccomp()?;

    chdir("/app").into_diagnostic()?;

    if test {
        return Ok(());
    }

    exec_in_sandbox(is_install, &manifest.install_commands, bin, extra_args)
}

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
            create_dir_all(&target).into_diagnostic()?;
            mount(
                Some(p),
                  target.to_str().unwrap(),
                  None::<&str>,
                  MsFlags::MS_BIND | MsFlags::MS_REC | MsFlags::MS_RDONLY,
                  None::<&str>,
            ).into_diagnostic()?;
        }
    }

    let app_path = new_root.join("app");
    create_dir_all(&app_path).into_diagnostic()?;
    mount(
        Some(path),
          app_path.to_str().unwrap(),
          None::<&str>,
          MsFlags::MS_BIND | MsFlags::MS_REC,
          None::<&str>,
    ).into_diagnostic()?;

    let tmp_path = new_root.join("tmp");
    create_dir_all(&tmp_path).into_diagnostic()?;
    mount(
        Some("tmpfs"),
          tmp_path.to_str().unwrap(),
          Some("tmpfs"),
          MsFlags::empty(),
          None::<&str>,
    ).into_diagnostic()?;

    if sandbox.gui || sandbox.full_gui {
        // X11
        if Path::new("/tmp/.X11-unix").exists() {
            let x11_path = new_root.join("tmp/.X11-unix");
            create_dir_all(&x11_path).into_diagnostic()?;
            mount(
                Some("/tmp/.X11-unix"),
                  x11_path.to_str().unwrap(),
                  None::<&str>,
                  MsFlags::MS_BIND | MsFlags::MS_REC,
                  None::<&str>,
            ).into_diagnostic()?;
        }

        // Wayland
        if let Ok(runtime_dir) = env::var("XDG_RUNTIME_DIR") {
            let wayland_socket = format!("{}/wayland-0", runtime_dir);
            if Path::new(&wayland_socket).exists() {
                let target = new_root.join(runtime_dir.trim_start_matches('/')).join("wayland-0");
                create_dir_all(target.parent().unwrap()).into_diagnostic()?;
                mount(
                    Some(wayland_socket.as_str()),
                      target.to_str().unwrap(),
                      None::<&str>,
                      MsFlags::MS_BIND | MsFlags::MS_REC,
                      None::<&str>,
                ).into_diagnostic()?;
            }
        }

        // D-Bus
        if let Ok(runtime_dir) = env::var("XDG_RUNTIME_DIR") {
            let bus_socket = format!("{}/bus", runtime_dir);
            if Path::new(&bus_socket).exists() {
                let target = new_root.join(runtime_dir.trim_start_matches('/')).join("bus");
                create_dir_all(target.parent().unwrap()).into_diagnostic()?;
                mount(
                    Some(bus_socket.as_str()),
                      target.to_str().unwrap(),
                      None::<&str>,
                      MsFlags::MS_BIND | MsFlags::MS_REC,
                      None::<&str>,
                ).into_diagnostic()?;
            }
        }

        // PulseAudio
        if let Ok(runtime_dir) = env::var("XDG_RUNTIME_DIR") {
            let pulse_dir = format!("{}/pulse", runtime_dir);
            if Path::new(&pulse_dir).exists() {
                let target = new_root.join(runtime_dir.trim_start_matches('/')).join("pulse");
                create_dir_all(target.parent().unwrap()).into_diagnostic()?;
                mount(
                    Some(pulse_dir.as_str()),
                      target.to_str().unwrap(),
                      None::<&str>,
                      MsFlags::MS_BIND | MsFlags::MS_REC,
                      None::<&str>,
                ).into_diagnostic()?;
            }
        }

        // /dev/dri
        if Path::new("/dev/dri").exists() {
            let target = new_root.join("dev/dri");
            create_dir_all(&target).into_diagnostic()?;
            mount(
                Some("/dev/dri"),
                  target.to_str().unwrap(),
                  None::<&str>,
                  MsFlags::MS_BIND | MsFlags::MS_REC,
                  None::<&str>,
            ).into_diagnostic()?;
        }
    }

    if sandbox.dev {
        let dev_path = new_root.join("dev");
        create_dir_all(&dev_path).into_diagnostic()?;
        mount(
            Some("tmpfs"),
              dev_path.to_str().unwrap(),
              Some("tmpfs"),
              MsFlags::empty(),
              None::<&str>,
        ).into_diagnostic()?;

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
            create_dir_all(parent).into_diagnostic()?;
        }
        if Path::new(fs_p).exists() {
            mount(
                Some(fs_p.as_str()),
                  target.to_str().unwrap(),
                  None::<&str>,
                  MsFlags::MS_BIND | MsFlags::MS_REC,
                  None::<&str>,
            ).into_diagnostic()?;
        }
    }

    let proc_path = new_root.join("proc");
    create_dir_all(&proc_path).into_diagnostic()?;
    mount(Some("proc"), proc_path.to_str().unwrap(), Some("proc"), MsFlags::empty(), None::<&str>).into_diagnostic()?;

    let sys_path = new_root.join("sys");
    create_dir_all(&sys_path).into_diagnostic()?;
    mount(Some("sysfs"), sys_path.to_str().unwrap(), Some("sysfs"), MsFlags::empty(), None::<&str>).into_diagnostic()?;

    if let Some(d) = display {
        env::set_var("DISPLAY", d);
    }

    Ok(())
}

fn pivot_and_chdir(new_root: &Path) -> Result<()> {
    chdir(new_root).into_diagnostic()?;
    create_dir_all("old_root").into_diagnostic()?;
    pivot_root(".", "old_root").into_diagnostic()?;
    chdir("/").into_diagnostic()?;
    umount2("/old_root", MntFlags::MNT_DETACH).into_diagnostic()?;
    Ok(())
}

fn set_resource_limits() -> Result<()> {
    setrlimit(Resource::RLIMIT_CPU, 60, 60).into_diagnostic()?;
    let mem_limit = 512 * 1024 * 1024;
    setrlimit(Resource::RLIMIT_AS, mem_limit, mem_limit).into_diagnostic()?;
    setrlimit(Resource::RLIMIT_NPROC, 1024, 1024).into_diagnostic()?;
    Ok(())
}

fn setup_landlock(manifest: &Manifest) -> Result<()> {
    let abi = ABI::V1;
    let mut ruleset = Ruleset::default()
    .handle_access(AccessFs::from_all(abi))
    .map_err(|e| miette!("Landlock error: {}", e))?
    .create()
    .map_err(|e| miette!("Landlock error: {}", e))?;

    let ro_access = AccessFs::Execute | AccessFs::ReadFile | AccessFs::ReadDir;

    for path in &["/usr", "/lib", "/lib64", "/bin", "/etc"] {
        if Path::new(path).exists() {
            ruleset = ruleset.add_rule(PathBeneath::new(PathFd::new(path).map_err(|e| miette!("Landlock error: {}", e))?, ro_access))
            .map_err(|e| miette!("Landlock error: {}", e))?;
        }
    }

    for path in &["/proc", "/sys"] {
        if Path::new(path).exists() {
            ruleset = ruleset.add_rule(PathBeneath::new(PathFd::new(path).map_err(|e| miette!("Landlock error: {}", e))?, AccessFs::ReadFile | AccessFs::ReadDir))
            .map_err(|e| miette!("Landlock error: {}", e))?;
        }
    }

    ruleset = ruleset.add_rule(PathBeneath::new(PathFd::new("/app").map_err(|e| miette!("Landlock error: {}", e))?, AccessFs::from_all(abi)))
    .map_err(|e| miette!("Landlock error: {}", e))?;
    ruleset = ruleset.add_rule(PathBeneath::new(PathFd::new("/tmp").map_err(|e| miette!("Landlock error: {}", e))?, AccessFs::from_all(abi)))
    .map_err(|e| miette!("Landlock error: {}", e))?;

    if manifest.sandbox.dev && Path::new("/dev").exists() {
        ruleset = ruleset.add_rule(PathBeneath::new(PathFd::new("/dev").map_err(|e| miette!("Landlock error: {}", e))?, AccessFs::from_all(abi)))
        .map_err(|e| miette!("Landlock error: {}", e))?;
    }

    for fs_p in &manifest.sandbox.filesystem {
        if Path::new(fs_p).exists() {
            ruleset = ruleset.add_rule(PathBeneath::new(PathFd::new(fs_p).map_err(|e| miette!("Landlock error: {}", e))?, AccessFs::from_all(abi)))
            .map_err(|e| miette!("Landlock error: {}", e))?;
        }
    }

    ruleset.restrict_self().map_err(|e| miette!("Landlock error: {}", e))?;
    Ok(())
}

fn setup_seccomp() -> Result<()> {
    let ctx = SeccompContext::default(Action::Allow).map_err(|e| miette!("Seccomp error: {}", e))?;
    ctx.load().map_err(|e| miette!("Seccomp error: {}", e))?;
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
            CString::new("/bin/sh").map_err(|e| miette!("CString error: {}", e))?,
         vec![CString::new("-c").map_err(|e| miette!("CString error: {}", e))?, CString::new(install_cmd).map_err(|e| miette!("CString error: {}", e))?],
        )
    } else {
        let bin_path = format!("/app/{}", bin.expect("Bin required"));
        let mut a = vec![CString::new(bin_path.as_str()).map_err(|e| miette!("CString error: {}", e))?];
        for arg in extra_args {
            a.push(CString::new(arg).map_err(|e| miette!("CString error: {}", e))?);
        }
        (CString::new(bin_path).map_err(|e| miette!("CString error: {}", e))?, a)
    };

    let args_ptr: Vec<&CStr> = args_c.iter().map(|c| c.as_c_str()).collect();
    execve(&cmd, &args_ptr, &[] as &[&CStr]).map_err(|e| miette!("execve error: {}", e))?;
    unreachable!()
}
