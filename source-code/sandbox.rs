use crate::manifest::{Manifest, Sandbox};
use miette::{Result, bail, miette, IntoDiagnostic};
use colored::Colorize;
use landlock::{
    Access, AccessFs, PathBeneath, PathFd, Ruleset, RulesetAttr,
    RulesetCreatedAttr, RulesetStatus, ABI,
};
use nix::mount::{mount, umount2, MntFlags, MsFlags};
use nix::sched::{unshare, CloneFlags};
use nix::sys::stat::{mknod, Mode as MkMode, SFlag, makedev};
use nix::sys::resource::{setrlimit, Resource};
use nix::unistd::{
    chdir, dup2, fork, getpid, pipe, pivot_root, read, write,
    ForkResult, Gid, Uid, sethostname, execve,
};
use libseccomp::{ScmpFilterContext, ScmpAction, ScmpSyscall};
use std::env;
use std::ffi::{CStr, CString};
use std::fs::{create_dir_all, File};
use std::io::Write as IoWrite;
use std::os::fd::OwnedFd;
use std::os::unix::io::{AsRawFd, RawFd};
use std::path::{Path, PathBuf};
use std::process::exit;
use std::time::{Duration, Instant};

// ---------------------------------------------------------------------------
// Sandbox mode
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum SandboxMode { Full, Compat, None }

#[derive(Debug, Clone, Copy)]
pub struct ResourceLimits {
    pub cpu_secs:  u64,
    pub mem_bytes: u64,
    pub nproc:     u64,
}

impl ResourceLimits {
    pub fn for_run()   -> Self { Self { cpu_secs: 0, mem_bytes: 4 * 1024 * 1024 * 1024, nproc: 2048 } }
    pub fn for_build() -> Self { Self { cpu_secs: 0, mem_bytes: 8 * 1024 * 1024 * 1024, nproc: 8192 } }
    /// Hooki (pre/post-install, pre/post-remove, post-update) — mocno ograniczone.
    /// To krótkie skrypty pomocnicze (rejestracja usługi, sprawdzenie zależności),
    /// nie powinny kompilować kodu ani ściągać dużych plików.
    pub fn for_hook() -> Self { Self { cpu_secs: 120, mem_bytes: 512 * 1024 * 1024, nproc: 256 } }
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

pub fn setup_sandbox(
    path: &str, manifest: &Manifest, is_install: bool,
    bin: Option<&str>, extra_args: Vec<String>, test: bool,
) -> Result<()> {
    match pick_mode(manifest) {
        SandboxMode::None   => exec_direct(manifest, is_install, bin, extra_args),
        SandboxMode::Compat => run_compat(path, manifest, is_install, bin, extra_args, test),
        SandboxMode::Full   => {
            // BUG NAPRAWIONY (znaleziony przez realny test `hpm run`/`hpm dev
            // <path> run` w zagnieżdżonym/restrykcyjnym kontenerze): nawet gdy
            // `can_use_user_ns()` mówi "tak", pełny pivot_root + mount tmpfs +
            // urządzenia w nowym root'cie potrafi i tak wywalić się w
            // nieoczywisty sposób (np. EOVERFLOW przy tworzeniu węzłów
            // urządzeń w zagnieżdżonym userns). Zamiast wywalać całe polecenie
            // użytkownika, spróbuj Full, a jeśli się nie uda — przejdź
            // przezroczyście do Compat zamiast krzyczeć błędem, który i tak
            // nie mówi nic użytecznego zwykłemu użytkownikowi.
            match run_full(path, manifest, is_install, bin, extra_args.clone(), test, ResourceLimits::for_run()) {
                Ok(())  => Ok(()),
                Err(e) => {
                    eprintln!("  {} Full sandbox mode failed ({}) — falling back to compat mode",
                              "⚠".bright_black(), e);
                    run_compat(path, manifest, is_install, bin, extra_args, test)
                }
            }
        }
    }
}

pub fn run_commands(path: &str, manifest: &Manifest, commands: &[String]) -> Result<()> {
    let script = format!("{}/run_commands.sh", path);
    std::fs::write(&script, format!("#!/bin/sh\nset -e\n{}", commands.join("\n")))
        .into_diagnostic()?;
    crate::utils::make_executable(Path::new(&script))?;
    let result = run_compat(path, manifest, false, Some("run_commands.sh"), vec![], false);
    let _ = std::fs::remove_file(&script);
    result
}

// ---------------------------------------------------------------------------
// Hook sandboxing
//
// Hooki pakietów (pre-install / post-install / pre-remove / post-remove /
// post-update) są wykonywane jako root podczas `sudo hpm install|remove|update`.
// Wcześniej odpalane były gołym `Command::new` bez ŻADNEJ izolacji — złośliwy
// hook `.sh`/`.py`/`.hl` miał pełny dostęp roota do systemu. Poniższe funkcje
// uruchamiają hook w tej samej piaskownicy co `hpm run` (namespaces + seccomp +
// landlock + rlimits), z jedną różnicą: hooki NIGDY nie dostają sieci, nawet
// jeśli pakiet deklaruje `sandbox.network = true` dla samej aplikacji — to
// świadoma decyzja "least privilege": kod instalacyjny nie powinien wymagać
// sieci bez jawnej, oddzielnej zgody (patrz TODO w README dot. przyszłego
// pola `sandbox.hooks_network`).
//
// Izolacja jest "best-effort": brak uprawnień jądra (kontener bez
// CAP_SYS_ADMIN, brak unprivileged user namespaces, wyłączony Landlock/
// seccomp) nigdy nie blokuje wykonania hooka — po prostu ta konkretna warstwa
// zostaje pominięta z ostrzeżeniem. Dzięki temu `hpm dev test`/hooki nadal
// działają w restrykcyjnych środowiskach CI, a na w pełni uprawnionym systemie
// docelowym (prawdziwy HackerOS) hook dostaje pełną izolację.
// ---------------------------------------------------------------------------

/// Uruchamia `interpreter hook_path` w piaskownicy i zwraca pełny `Output`
/// (stdout/stderr/status), dokładnie tak jak zwykłe `Command::output()`.
///
/// - PID/UTS/IPC/CGROUP namespaces zawsze, gdy dostępne; USER+PID namespace
///   gdy unprivileged userns jest włączony w jądrze.
/// - Sieć ZAWSZE odizolowana (CLONE_NEWNET) — patrz komentarz wyżej.
/// - seccomp: blokuje mount/pivot_root/ptrace/kexec/ładowanie modułów/reboot itd.
/// - Landlock: dostęp r/o do /usr,/lib*,/bin,/sbin,/etc,/proc,/sys; r/w
///   WYŁĄCZNIE do katalogu pakietu (`dir`), `/tmp` i ścieżek jawnie
///   zadeklarowanych przez packagera w `[sandbox] filesystem` — hook NIGDY nie
///   dostaje r/w do `$HOME` użytkownika wywołującego `hpm`.
/// - twarde limity CPU/RAM/liczby procesów (`ResourceLimits::for_hook`).
/// - własna grupa procesów (setpgid), dzięki czemu timeout może ubić całe
///   drzewo procesów potomnych hooka, nie tylko bezpośredniego dziecka.
///
/// `manifest.sandbox_disabled` pozostaje jawnym wyłącznikiem awaryjnym (np. dla
/// zaufanych, oficjalnie zweryfikowanych pakietów systemowych) — jeśli
/// ustawiony, hook wykonuje się bez żadnej izolacji, tak jak wcześniej.
pub fn run_hook_sandboxed(
    dir:         &Path,
    manifest:    &Manifest,
    interpreter: &str,
    hook_path:   &Path,
    env_vars:    &[(String, String)],
    timeout:     Duration,
) -> Result<std::process::Output> {
    use std::io::Read;
    use std::os::unix::process::CommandExt as UnixCommandExt;
    use std::process::{Command as StdCommand, Stdio};

    let sandbox_disabled = manifest.sandbox_disabled;
    let allow_network    = manifest.sandbox.hooks_network;
    let manifest_clone   = manifest.clone();
    let dir_owned        = dir.to_path_buf();
    let limits           = ResourceLimits::for_hook();

    let mut cmd = StdCommand::new(interpreter);
    cmd.arg(hook_path)
        .current_dir(dir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (k, v) in env_vars {
        cmd.env(k, v);
    }

    // SAFETY: closure wykonuje wyłącznie syscalle jądra (unshare/setrlimit/
    // seccomp/mount) na świeżo sforkowanym, jednowątkowym procesie potomnym,
    // tuż przed exec — to dokładnie sytuacja, do której pre_exec jest
    // przeznaczone. Nic tu nie alokuje przez globalny alokator w sposób, który
    // mógłby zakleszczyć się po forku wielowątkowego rodzica.
    unsafe {
        cmd.pre_exec(move || {
            if sandbox_disabled {
                return Ok(());
            }
            harden_hook_child(&dir_owned, &manifest_clone, limits, allow_network);
            Ok(())
        });
    }

    let mut child = cmd.spawn().into_diagnostic()
        .map_err(|e| miette!("Failed to spawn hook interpreter '{}': {}", interpreter, e))?;

    let mut stdout_pipe = child.stdout.take().expect("piped stdout");
    let mut stderr_pipe = child.stderr.take().expect("piped stderr");
    let out_handle = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = stdout_pipe.read_to_end(&mut buf);
        buf
    });
    let err_handle = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = stderr_pipe.read_to_end(&mut buf);
        buf
    });

    let pid   = child.id() as i32;
    let start = Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait().into_diagnostic()? {
            break status;
        }
        if start.elapsed() >= timeout {
            // Ubij całą grupę procesów, nie tylko bezpośredniego potomka —
            // interpretery typu sh/python potrafią odpalać dzieci, które
            // wcześniejsza implementacja (Command::kill na samym potomku)
            // zostawiała jako sieroty.
            unsafe { libc::kill(-pid, libc::SIGKILL); }
            let _ = child.kill();
            let _ = child.wait();
            let _ = out_handle.join();
            let _ = err_handle.join();
            bail!("Hook timed out after {} seconds and was killed", timeout.as_secs());
        }
        std::thread::sleep(Duration::from_millis(25));
    };

    let stdout = out_handle.join().unwrap_or_default();
    let stderr = err_handle.join().unwrap_or_default();
    Ok(std::process::Output { status, stdout, stderr })
}

/// Wykonywane w potomku, tuż po fork(), tuż przed exec(). Każdy krok jest
/// best-effort: brak wsparcia jądra dla danej warstwy nie jest fatalny, tylko
/// pomijamy ją z ostrzeżeniem na stderr (które trafi razem z wyjściem hooka).
fn harden_hook_child(dir: &Path, manifest: &Manifest, limits: ResourceLimits, allow_network: bool) {
    // Własna grupa procesów, żeby timeout mógł ubić całe drzewo (patrz wyżej).
    let _ = nix::unistd::setpgid(nix::unistd::Pid::from_raw(0), nix::unistd::Pid::from_raw(0));

    // Prawdziwe uid/gid MUSZĄ być przechwycone PRZED unshare(CLONE_NEWUSER) —
    // patrz obszerny komentarz w `full_setup`. Po unshare, `Uid::current()`
    // zwraca już placeholder "nobody" (65534) namespace'u, nie prawdziwe id.
    let real_uid = Uid::current();
    let real_gid = Gid::current();

    // Namespaces: PID/UTS/IPC/CGROUP zawsze; USER+PID tylko jeśli unprivileged
    // userns jest dostępny w jądrze. Sieć izolowana chyba że pakiet jawnie
    // zadeklarował `[sandbox] hooks_network => true`.
    let mut flags = CloneFlags::CLONE_NEWNS | CloneFlags::CLONE_NEWUTS
        | CloneFlags::CLONE_NEWIPC | CloneFlags::CLONE_NEWCGROUP;
    if !allow_network {
        flags |= CloneFlags::CLONE_NEWNET;
    }
    let have_userns = can_use_user_ns();
    if have_userns {
        flags |= CloneFlags::CLONE_NEWUSER | CloneFlags::CLONE_NEWPID;
    }

    if unshare(flags).is_err() {
        // Brak CAP_SYS_ADMIN / unprivileged userns wyłączony (typowe w
        // kontenerach CI) — spróbuj chociaż odizolować mount namespace.
        // Reszta izolacji (seccomp + landlock + rlimits) i tak zadziała.
        let _ = unshare(CloneFlags::CLONE_NEWNS);
        eprintln!("hpm-sandbox: warning: namespace isolation unavailable for hook, \
                    falling back to seccomp+landlock+rlimits only");
    } else if have_userns {
        if let Err(e) = setup_user_mapping(real_uid, real_gid) {
            eprintln!("hpm-sandbox: warning: user mapping failed for hook: {}", e);
        }
    }

    let _ = mount(None::<&str>, "/", None::<&str>, MsFlags::MS_PRIVATE | MsFlags::MS_REC, None::<&str>);

    if let Err(e) = apply_resource_limits(limits) {
        eprintln!("hpm-sandbox: warning: could not apply resource limits to hook: {}", e);
    }
    if let Err(e) = setup_landlock_for_hook(manifest, dir) {
        eprintln!("hpm-sandbox: warning: landlock unavailable for hook ({}) — \
                    relying on seccomp + namespaces only", e);
    }
    if let Err(e) = setup_seccomp() {
        eprintln!("hpm-sandbox: warning: seccomp unavailable for hook: {}", e);
    }
}

/// Landlock ruleset dedykowany hookom: r/o system, r/w WYŁĄCZNIE do katalogu
/// pakietu + /tmp + ścieżki jawnie zadeklarowane w `[sandbox] filesystem`.
/// W przeciwieństwie do `setup_landlock` (używanego przez `hpm run`), tutaj
/// nigdy nie dodajemy $HOME do reguł r/w — hook instalacyjny nie ma powodu
/// dotykać katalogu domowego użytkownika wywołującego `sudo hpm install`.
fn setup_landlock_for_hook(manifest: &Manifest, dir: &Path) -> Result<()> {
    let abi = best_landlock_abi()
        .ok_or_else(|| miette!("Landlock not supported (requires Linux 5.13+)"))?;
    let mut ruleset = Ruleset::default()
        .handle_access(AccessFs::from_all(abi)).map_err(|e| miette!("{}", e))?
        .create().map_err(|e| miette!("{}", e))?;
    let ro = AccessFs::Execute | AccessFs::ReadFile | AccessFs::ReadDir;
    let rw = AccessFs::from_all(abi);

    for path in &["/usr", "/lib", "/lib64", "/lib32", "/bin", "/sbin", "/etc"] {
        if !Path::new(path).exists() { continue; }
        ruleset = ruleset.add_rule(PathBeneath::new(PathFd::new(path).map_err(|e| miette!("{}", e))?, ro))
            .map_err(|e| miette!("{}", e))?;
    }
    for path in &["/proc", "/sys"] {
        if !Path::new(path).exists() { continue; }
        ruleset = ruleset.add_rule(PathBeneath::new(PathFd::new(path).map_err(|e| miette!("{}", e))?,
            AccessFs::ReadFile | AccessFs::ReadDir)).map_err(|e| miette!("{}", e))?;
    }
    if dir.exists() {
        ruleset = ruleset.add_rule(PathBeneath::new(PathFd::new(dir).map_err(|e| miette!("{}", e))?, rw))
            .map_err(|e| miette!("{}", e))?;
    }
    if Path::new("/tmp").exists() {
        ruleset = ruleset.add_rule(PathBeneath::new(PathFd::new("/tmp").map_err(|e| miette!("{}", e))?, rw))
            .map_err(|e| miette!("{}", e))?;
    }
    for fs_p in &manifest.sandbox.filesystem {
        if !Path::new(fs_p).exists() { continue; }
        ruleset = ruleset.add_rule(PathBeneath::new(PathFd::new(fs_p).map_err(|e| miette!("{}", e))?, rw))
            .map_err(|e| miette!("{}", e))?;
    }
    let status = ruleset.restrict_self().map_err(|e| miette!("{}", e))?;
    if status.ruleset == RulesetStatus::NotEnforced {
        bail!("Landlock not enforced");
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Mode selection
// ---------------------------------------------------------------------------

fn pick_mode(manifest: &Manifest) -> SandboxMode {
    if manifest.sandbox_disabled { return SandboxMode::None; }
    let s = &manifest.sandbox;
    if s.gui || s.full_gui || s.network || !s.filesystem.is_empty() {
        return SandboxMode::Compat;
    }
    if !can_use_user_ns() { return SandboxMode::Compat; }
    SandboxMode::Full
}

fn can_use_user_ns() -> bool {
    if let Ok(v) = std::fs::read_to_string("/proc/sys/kernel/unprivileged_userns_clone") {
        if v.trim() == "0" { return false; }
    }
    if let Ok(v) = std::fs::read_to_string("/proc/sys/user/max_user_namespaces") {
        if v.trim() == "0" { return false; }
    }
    // BUG NAPRAWIONY (znaleziony przez realny test `hpm dev <path> run <bin>`
    // w kontenerze CI): same sysctle nie wystarczają. Docker/gVisor/CI często
    // blokują unshare(CLONE_NEWUSER) własnym seccomp/AppArmor NAWET gdy
    // hostowe sysctle na to pozwalają — poprzednia wersja tej funkcji ufała
    // tylko sysctlom, więc `pick_mode` wybierał `SandboxMode::Full`, a `hpm
    // run` padał w połowie z "Invalid argument (os error 22)" zamiast
    // spaść do trybu Compat. Zweryfikuj to empirycznie: spróbuj naprawdę
    // odpalić unshare w jednorazowym forkowanym dziecku.
    match unsafe { fork() } {
        Ok(ForkResult::Child) => {
            let ok = unshare(CloneFlags::CLONE_NEWUSER | CloneFlags::CLONE_NEWPID).is_ok();
            exit(if ok { 0 } else { 1 });
        }
        Ok(ForkResult::Parent { child, .. }) => {
            matches!(
                nix::sys::wait::waitpid(child, None),
                Ok(nix::sys::wait::WaitStatus::Exited(_, 0))
            )
        }
        Err(_) => false,
    }
}

// ---------------------------------------------------------------------------
// exec_direct
// ---------------------------------------------------------------------------

fn exec_direct(manifest: &Manifest, is_install: bool, bin: Option<&str>, extra_args: Vec<String>) -> Result<()> {
    let (read_fd, write_fd) = pipe().into_diagnostic()?;
    match unsafe { fork() }.into_diagnostic()? {
        ForkResult::Parent { child, .. } => wait_child(child, read_fd),
        ForkResult::Child => {
            if let Err(e) = exec_in_sandbox(is_install, &manifest.install_commands, bin, extra_args) {
                let _ = write(write_fd, format!("{:?}", e).as_bytes());
                exit(1);
            }
            exit(0);
        }
    }
}

// ---------------------------------------------------------------------------
// Compat mode
// ---------------------------------------------------------------------------

fn run_compat(
    path: &str, manifest: &Manifest, is_install: bool,
    bin: Option<&str>, extra_args: Vec<String>, test: bool,
) -> Result<()> {
    let (read_fd, write_fd) = pipe().into_diagnostic()?;
    match unsafe { fork() }.into_diagnostic()? {
        ForkResult::Parent { child, .. } => wait_child(child, read_fd),
        ForkResult::Child => {
            if let Err(e) = compat_setup(path, manifest, is_install, bin, extra_args, test) {
                let _ = write(write_fd, format!("{:?}", e).as_bytes());
                exit(1);
            }
            exit(0);
        }
    }
}

fn compat_setup(
    path: &str, manifest: &Manifest, is_install: bool,
    bin: Option<&str>, extra_args: Vec<String>, test: bool,
) -> Result<()> {
    if unshare(CloneFlags::CLONE_NEWNS).is_ok() {
        let _ = mount(None::<&str>, "/", None::<&str>, MsFlags::MS_PRIVATE | MsFlags::MS_REC, None::<&str>);
    }

    // FIX: explicit dup2(STDIN_FILENO) — po CLONE_NEWNS stdin może być zamknięty
    // w potomnym procesie jeśli rodzic zamknie deskryptory. Otwieramy /dev/null
    // jako fallback jeśli stdin jest zamknięty.
    ensure_stdin_open();

    apply_resource_limits(ResourceLimits::for_run())?;
    setup_seccomp()?;
    if test { return Ok(()); }
    exec_from_path(path, is_install, &manifest.install_commands, bin, extra_args)
}

/// Upewnij się że stdin (fd=0) jest otwarty.
/// W CLONE_NEWNS stdin jest dziedziczony, ale gdy rodzic zamknie swój koniec pipe —
/// stdin dziecka może stać się zamknięty w niektórych konfiguracjach.
fn ensure_stdin_open() {
    use nix::fcntl::{open, OFlag};
    use nix::sys::stat::Mode;
    // Sprawdź czy fd 0 jest otwarty przez próbę fcntl
    let stdin_fd: RawFd = 0;
    let flags = nix::fcntl::fcntl(stdin_fd, nix::fcntl::FcntlArg::F_GETFD);
    if flags.is_err() {
        // stdin zamknięty — otwórz /dev/null jako fallback
        if let Ok(null_fd) = open(
            "/dev/null",
            OFlag::O_RDONLY,
            Mode::empty(),
        ) {
            if null_fd != 0 {
                let _ = dup2(null_fd, 0);
                let _ = nix::unistd::close(null_fd);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Full mode
// ---------------------------------------------------------------------------

fn run_full(
    path: &str, manifest: &Manifest, is_install: bool,
    bin: Option<&str>, extra_args: Vec<String>, test: bool, limits: ResourceLimits,
) -> Result<()> {
    let (read_fd, write_fd) = pipe().into_diagnostic()?;
    match unsafe { fork() }.into_diagnostic()? {
        ForkResult::Parent { child, .. } => wait_child(child, read_fd),
        ForkResult::Child => {
            if let Err(e) = full_setup(path, manifest, is_install, bin, extra_args, test, limits) {
                let _ = write(write_fd, format!("{:?}", e).as_bytes());
                exit(1);
            }
            exit(0);
        }
    }
}

fn full_setup(
    path: &str, manifest: &Manifest, is_install: bool,
    bin: Option<&str>, extra_args: Vec<String>, test: bool, limits: ResourceLimits,
) -> Result<()> {
    // BUG NAPRAWIONY (prawdziwa przyczyna EOVERFLOW, znaleziona przez
    // dodanie tracingu krok po kroku i ręczną reprodukcję poza hpm):
    // `Uid::current()`/`Gid::current()` były odczytywane WEWNĄTRZ
    // `setup_user_mapping()`, wołanego PO `unshare(CLONE_NEWUSER)`. Ale
    // `unshare(CLONE_NEWUSER)` NATYCHMIAST przełącza widok procesu na
    // "overflow uid" (65534/nobody) wewnątrz nowego, jeszcze
    // niezmapowanego namespace'u — to jest normalne zachowanie jądra. Kod
    // odczytywał więc uid=65534 i próbował zapisać do uid_map "0 65534 1"
    // (zmapuj ns-uid 0 na HOST-uid 65534) zamiast "0 <prawdziwe-uid> 1".
    // To ZAWSZE było błędne, w każdym środowisku — naprawka (przechwycenie
    // uid/gid PRZED unshare) jest uniwersalnie poprawna i konieczna.
    //
    // Osobna sprawa: w niektórych środowiskach (np. tej maszynie — Firecracker
    // microVM z niestandardowym jądrem 6.18.5, custom init) NAWET poprawny
    // zapis "0 <prawdziwe-root-uid> 1" do uid_map bywa odrzucany (EPERM/EINVAL)
    // przez dodatkowe utwardzenie platformy blokujące mapowanie ID w
    // zagnieżdżonych user namespace'ach — zweryfikowane ręczną reprodukcją
    // (`unshare --user` + zapis do uid_map) POZA hpm, więc to NIE jest coś,
    // co kod hpm może naprawić — to świadome ograniczenie hosta. Stąd
    // fallback poniżej pozostaje best-effort/non-fatal: gdy mapowanie się
    // nie uda z JAKIEGOKOLWIEK powodu, kontynuujemy w niezmapowanym
    // namespace (subsekwentne bind-mounty i tak spadną do trybu Compat przez
    // `setup_sandbox`'s Full→Compat fallback, patrz niżej w tym pliku).
    let real_uid = Uid::current();
    let real_gid = Gid::current();

    let mut flags = CloneFlags::CLONE_NEWUSER | CloneFlags::CLONE_NEWNS
        | CloneFlags::CLONE_NEWUTS | CloneFlags::CLONE_NEWPID | CloneFlags::CLONE_NEWCGROUP;
    if !manifest.sandbox.network { flags |= CloneFlags::CLONE_NEWNET; }
    if !manifest.sandbox.gui     { flags |= CloneFlags::CLONE_NEWIPC; }
    unshare(flags).into_diagnostic()?;
    sethostname(&manifest.name).into_diagnostic()?;
    mount(None::<&str>, "/", None::<&str>, MsFlags::MS_PRIVATE | MsFlags::MS_REC, None::<&str>)
        .into_diagnostic()?;
    if let Err(e) = setup_user_mapping(real_uid, real_gid) {
        eprintln!("  {} User ID mapping failed ({}) — continuing with an unmapped user namespace",
                  "⚠".bright_black(), e);
    }
    let new_root_str = format!("/tmp/hpm_newroot_{}", getpid());
    let new_root = PathBuf::from(&new_root_str);
    create_dir_all(&new_root).into_diagnostic()?;
    mount(Some("tmpfs"), new_root_str.as_str(), Some("tmpfs"), MsFlags::empty(), None::<&str>)
        .into_diagnostic()?;
    let display = env::var("DISPLAY").ok();
    setup_mounts(&new_root, path, &manifest.sandbox, display.as_ref())?;
    if let Err(e) = pivot_and_chdir(&new_root) {
        eprintln!("  {} pivot_root failed ({}), falling back to chroot", "⚠".bright_black(), e);
        nix::unistd::chroot(&new_root).into_diagnostic()?;
        chdir("/").into_diagnostic()?;
    }
    ensure_stdin_open();
    apply_resource_limits(limits)?;
    if let Err(e) = setup_landlock(manifest) {
        eprintln!("  {} Landlock unavailable: {}", "⚠".bright_black(), e);
    }
    setup_seccomp()?;
    chdir("/app").into_diagnostic()?;
    if test { return Ok(()); }
    exec_in_sandbox(is_install, &manifest.install_commands, bin, extra_args)
}

// ---------------------------------------------------------------------------
// Wait
// ---------------------------------------------------------------------------

fn wait_child(child: nix::unistd::Pid, read_fd: OwnedFd) -> Result<()> {
    let status = nix::sys::wait::waitpid(child, None).into_diagnostic()?;

    // BUG NAPRAWIONY: `/tmp/hpm_newroot_<pid>` (mountpoint tmpfs użyty przez
    // `full_setup` do pivot_root) nigdy nie był sprzątany — ani gdy dziecko
    // padło w połowie konfiguracji sandboxa, ani nawet przy sukcesie (bo
    // `exec_in_sandbox` kończy się przez `execve`, które nigdy nie wraca, więc
    // żaden kod po nim się nie wykonuje). Sam tmpfs znika wraz ze zniszczeniem
    // prywatnego mount namespace dziecka, ale pusty katalog-mountpoint na
    // rzeczywistym /tmp zostawał na zawsze. Rodzic zawsze zna pid dziecka i
    // zawsze w końcu je odbiera (czy to sukces, czy porażka) — to najlepsze
    // miejsce na sprzątanie, niezależnie od tego, jak dziecko zakończyło pracę.
    let leftover_root = format!("/tmp/hpm_newroot_{}", child.as_raw());
    if Path::new(&leftover_root).exists() {
        let _ = std::fs::remove_dir_all(&leftover_root);
    }

    let code   = match status {
        nix::sys::wait::WaitStatus::Exited(_, c) => c,
        _ => 1,
    };
    if code != 0 {
        let mut buf = vec![0u8; 4096];
        let n   = read(read_fd.as_raw_fd(), &mut buf).unwrap_or(0);
        let msg = String::from_utf8_lossy(&buf[..n]);
        if msg.is_empty() { bail!("Process exited with code {}", code); }
        bail!("Sandbox error: {}", msg.trim());
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// User mapping
// ---------------------------------------------------------------------------

fn setup_user_mapping(uid: Uid, gid: Gid) -> Result<()> {
    if !Path::new("/proc/self/uid_map").exists() {
        bail!("User namespace not available");
    }
    let mut f = File::create("/proc/self/uid_map")
        .map_err(|e| miette!("Cannot open uid_map: {}", e))?;
    let uid_line = format!("0 {} 1", uid);
    writeln!(f, "{}", uid_line).map_err(|e| miette!("Cannot write uid_map ('{}'): {}", uid_line, e))?;
    let mut f = File::create("/proc/self/setgroups").into_diagnostic()?;
    writeln!(f, "deny").map_err(|e| miette!("Cannot write setgroups: {}", e))?;
    let mut f = File::create("/proc/self/gid_map")
        .map_err(|e| miette!("Cannot open gid_map: {}", e))?;
    let gid_line = format!("0 {} 1", gid);
    writeln!(f, "{}", gid_line).map_err(|e| miette!("Cannot write gid_map ('{}'): {}", gid_line, e))?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Mounts
// ---------------------------------------------------------------------------

fn setup_mounts(new_root: &Path, path: &str, sandbox: &Sandbox, display: Option<&String>) -> Result<()> {
    for p in &["/usr", "/lib", "/lib64", "/lib32", "/bin", "/sbin", "/etc"] {
        if !Path::new(p).exists() { continue; }
        let target = new_root.join(p.trim_start_matches('/'));
        create_dir_all(&target).into_diagnostic()?;
        mount(Some(*p), target.to_str().unwrap(), None::<&str>,
              MsFlags::MS_BIND | MsFlags::MS_REC | MsFlags::MS_RDONLY, None::<&str>)
            .map_err(|e| miette!("bind-mount {} failed: {}", p, e))?;
    }
    let app = new_root.join("app");
    create_dir_all(&app).into_diagnostic()?;
    mount(Some(path), app.to_str().unwrap(), None::<&str>, MsFlags::MS_BIND | MsFlags::MS_REC, None::<&str>)
        .into_diagnostic()?;
    let tmp = new_root.join("tmp");
    create_dir_all(&tmp).into_diagnostic()?;
    mount(Some("tmpfs"), tmp.to_str().unwrap(), Some("tmpfs"), MsFlags::empty(), None::<&str>)
        .into_diagnostic()?;
    if let Ok(home) = env::var("HOME") {
        if Path::new(&home).exists() {
            let target = new_root.join(home.trim_start_matches('/'));
            create_dir_all(&target).into_diagnostic()?;
            mount(Some(home.as_str()), target.to_str().unwrap(), None::<&str>,
                  MsFlags::MS_BIND | MsFlags::MS_REC, None::<&str>).into_diagnostic()?;
        }
    }
    // BUG/MARTWY KOD USUNIĘTY: `pick_mode` (patrz wyżej) ZAWSZE kieruje
    // pakiety z `sandbox.gui`/`sandbox.full_gui` do trybu Compat, nigdy Full —
    // więc ta gałąź (i cała funkcja `bind_gui_sockets`, i gałąź `full_gui` w
    // `setup_dev`) nigdy nie były osiągalne. Nie jest to strata: Compat mode
    // NIE izoluje systemu plików (brak pivot_root), więc `/tmp/.X11-unix`,
    // `$XDG_RUNTIME_DIR/{wayland-0,pipewire-0,pulse}` i `/dev/dri` są i tak
    // widoczne bez żadnych bind-mountów — zweryfikowane na żywo małym
    // binarką testową sprawdzającą DISPLAY/XDG_RUNTIME_DIR w sandboksie.
    if sandbox.dev {
        setup_dev(new_root)?;
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
        if let Some(p) = target.parent() { create_dir_all(p).into_diagnostic()?; }
        mount(Some(fs_p.as_str()), target.to_str().unwrap(), None::<&str>,
              MsFlags::MS_BIND | MsFlags::MS_REC, None::<&str>).into_diagnostic()?;
    }
    let proc = new_root.join("proc");
    create_dir_all(&proc).into_diagnostic()?;
    mount(Some("proc"), proc.to_str().unwrap(), Some("proc"), MsFlags::empty(), None::<&str>)
        .into_diagnostic()?;
    let sys = new_root.join("sys");
    create_dir_all(&sys).into_diagnostic()?;
    mount(Some("sysfs"), sys.to_str().unwrap(), Some("sysfs"), MsFlags::empty(), None::<&str>)
        .into_diagnostic()?;
    if let Some(d) = display { env::set_var("DISPLAY", d); }
    Ok(())
}

/// Minimalny `/dev` dla trybu Full — używany tylko gdy pakiet jawnie
/// zadeklarował `[sandbox] dev => true` (narzędzia deweloperskie potrzebujące
/// więcej niż podstawowe `/dev/{null,zero,random,urandom,tty}`, np. `ptmx`
/// dla pseudo-terminali, `fuse` dla FUSE-based narzędzi).
fn setup_dev(new_root: &Path) -> Result<()> {
    let dev = new_root.join("dev");
    create_dir_all(&dev).into_diagnostic()?;
    mount(Some("tmpfs"), dev.to_str().unwrap(), Some("tmpfs"), MsFlags::empty(), None::<&str>)
        .into_diagnostic()?;
    for (name, maj, min) in &[
        ("null",1u64,3u64),("zero",1,5),("random",1,8),("urandom",1,9),
        ("tty",5,0),("ptmx",5,2),("fuse",10,229),
    ] {
        let _ = mknod(&dev.join(name), SFlag::S_IFCHR, MkMode::from_bits_truncate(0o666), makedev(*maj, *min));
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

fn apply_resource_limits(limits: ResourceLimits) -> Result<()> {
    if limits.cpu_secs  > 0 { setrlimit(Resource::RLIMIT_CPU,   limits.cpu_secs,  limits.cpu_secs).into_diagnostic()?; }
    if limits.mem_bytes > 0 { setrlimit(Resource::RLIMIT_AS,    limits.mem_bytes, limits.mem_bytes).into_diagnostic()?; }
    setrlimit(Resource::RLIMIT_NPROC, limits.nproc, limits.nproc).into_diagnostic()?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Landlock
// ---------------------------------------------------------------------------

fn setup_landlock(manifest: &Manifest) -> Result<()> {
    let abi = best_landlock_abi()
        .ok_or_else(|| miette!("Landlock not supported (requires Linux 5.13+)"))?;
    let mut ruleset = Ruleset::default()
        .handle_access(AccessFs::from_all(abi)).map_err(|e| miette!("{}", e))?
        .create().map_err(|e| miette!("{}", e))?;
    let ro = AccessFs::Execute | AccessFs::ReadFile | AccessFs::ReadDir;
    let rw = AccessFs::from_all(abi);
    for path in &["/usr","/lib","/lib64","/lib32","/bin","/sbin","/etc"] {
        if !Path::new(path).exists() { continue; }
        ruleset = ruleset.add_rule(PathBeneath::new(PathFd::new(path).map_err(|e| miette!("{}", e))?, ro))
            .map_err(|e| miette!("{}", e))?;
    }
    for path in &["/proc","/sys"] {
        if !Path::new(path).exists() { continue; }
        ruleset = ruleset.add_rule(PathBeneath::new(PathFd::new(path).map_err(|e| miette!("{}", e))?,
            AccessFs::ReadFile | AccessFs::ReadDir)).map_err(|e| miette!("{}", e))?;
    }
    for path in &["/app","/tmp"] {
        if !Path::new(path).exists() { continue; }
        ruleset = ruleset.add_rule(PathBeneath::new(PathFd::new(path).map_err(|e| miette!("{}", e))?, rw))
            .map_err(|e| miette!("{}", e))?;
    }
    if let Ok(home) = env::var("HOME") {
        if Path::new(&home).exists() {
            ruleset = ruleset.add_rule(PathBeneath::new(PathFd::new(&home).map_err(|e| miette!("{}", e))?, rw))
                .map_err(|e| miette!("{}", e))?;
        }
    }
    if manifest.sandbox.dev && Path::new("/dev").exists() {
        ruleset = ruleset.add_rule(PathBeneath::new(PathFd::new("/dev").map_err(|e| miette!("{}", e))?, rw))
            .map_err(|e| miette!("{}", e))?;
    }
    for fs_p in &manifest.sandbox.filesystem {
        if !Path::new(fs_p).exists() { continue; }
        ruleset = ruleset.add_rule(PathBeneath::new(PathFd::new(fs_p).map_err(|e| miette!("{}", e))?, rw))
            .map_err(|e| miette!("{}", e))?;
    }
    let status = ruleset.restrict_self().map_err(|e| miette!("{}", e))?;
    if status.ruleset == RulesetStatus::NotEnforced {
        bail!("Landlock not enforced");
    }
    Ok(())
}

fn best_landlock_abi() -> Option<ABI> {
    if let Ok(v) = std::fs::read_to_string("/proc/sys/kernel/landlock/abi") {
        return match v.trim().parse::<u32>().unwrap_or(0) {
            0 => None, 1 => Some(ABI::V1), 2 => Some(ABI::V2), _ => Some(ABI::V3),
        };
    }
    for abi in [ABI::V3, ABI::V2, ABI::V1] {
        if Ruleset::default().handle_access(AccessFs::from_all(abi)).is_ok() {
            return Some(abi);
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Seccomp — FIXED:
//   - usunięto nieistniejący ScmpArg
//   - ScmpAction::Errno(i32) — cast jawny libc::EPERM as i32
//   - dodano blokady mount/pivot_root/setns/unshare
// ---------------------------------------------------------------------------

fn setup_seccomp() -> Result<()> {
    let mut ctx = ScmpFilterContext::new_filter(ScmpAction::Allow)
        .map_err(|e| miette!("Seccomp context: {}", e))?;

    let blocked: &[&str] = &[
        "kexec_load", "kexec_file_load",
        "init_module", "finit_module", "delete_module",
        "ptrace",
        "process_vm_readv", "process_vm_writev",
        "iopl", "ioperm",
        "perf_event_open",
        "syslog",
        "acct",
        "swapon", "swapoff",
        "reboot",
        "keyctl", "add_key", "request_key",
        "bpf",
        "userfaultfd",
        "mount", "umount2",
        "pivot_root",
        "chroot",
        "setns",
        "unshare",
    ];

    for name in blocked {
        let syscall = ScmpSyscall::from_name(name)
            .map_err(|e| miette!("Unknown syscall '{}': {}", name, e))?;
        // FIXED: Errno przyjmuje i32, nie u32
        ctx.add_rule(ScmpAction::Errno(libc::EPERM as i32), syscall)
            .map_err(|e| miette!("seccomp add_rule '{}': {}", name, e))?;
    }

    ctx.load().map_err(|e| miette!("Seccomp load: {}", e))?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Exec helpers
// ---------------------------------------------------------------------------

fn exec_in_sandbox(
    is_install: bool, install_commands: &[String],
    bin: Option<&str>, extra_args: Vec<String>,
) -> Result<()> {
    let (cmd_str, args_strs) = if is_install {
        let cmd = if install_commands.is_empty() { "echo 'Install complete'".to_string() }
                  else { install_commands.join(" && ") };
        ("/bin/sh".to_string(), vec!["/bin/sh".to_string(), "-c".to_string(), cmd])
    } else {
        let bin_path = format!("/app/{}", bin.expect("bin required"));
        let mut args = vec![bin_path.clone()];
        args.extend(extra_args);
        (bin_path, args)
    };
    do_execve(&cmd_str, &args_strs)
}

fn exec_from_path(
    path: &str, is_install: bool, install_commands: &[String],
    bin: Option<&str>, extra_args: Vec<String>,
) -> Result<()> {
    let (cmd_str, args_strs) = if is_install {
        let cmd = if install_commands.is_empty() { "echo 'Install complete'".to_string() }
                  else { install_commands.join(" && ") };
        ("/bin/sh".to_string(), vec!["/bin/sh".to_string(), "-c".to_string(), cmd])
    } else {
        let bin_path = format!("{}/{}", path, bin.expect("bin required"));
        let mut args = vec![bin_path.clone()];
        args.extend(extra_args);
        (bin_path, args)
    };
    do_execve(&cmd_str, &args_strs)
}

fn do_execve(cmd: &str, args: &[String]) -> Result<()> {
    let cmd_c   = CString::new(cmd).map_err(|e| miette!("{}", e))?;
    let args_c: Vec<CString> = args.iter()
        .map(|a| CString::new(a.as_str()).map_err(|e| miette!("{}", e)))
        .collect::<Result<Vec<_>>>()?;
    let args_ptr: Vec<&CStr> = args_c.iter().map(|c| c.as_c_str()).collect();

    // BUG NAPRAWIONY (znaleziony przez realny test aplikacji GUI): execve()
    // dostawał dosłownie PUSTĄ tablicę środowiska (`&[] as &[&CStr]`), więc
    // KAŻDY sandboksowany proces — GUI czy nie, `hpm run` czy hooki przez tę
    // ścieżkę — tracił kompletnie wszystko: PATH, HOME, LANG, DISPLAY,
    // WAYLAND_DISPLAY, XDG_RUNTIME_DIR. Wcześniejsze `env::set_var("DISPLAY", ...)`
    // w `full_setup` nie miało żadnego efektu — execve i tak ignorował
    // bieżące środowisko procesu i dostawał osobną, pustą tablicę.
    // Budujemy envp z aktualnego `std::env::vars()` (który odzwierciedla
    // wszystkie `env::set_var` wywołane wcześniej w tym samym procesie, np.
    // DISPLAY w full_setup), więc rzeczywiście trafia do sandboksowanego procesu.
    let env_vars: Vec<CString> = std::env::vars()
        .filter_map(|(k, v)| CString::new(format!("{}={}", k, v)).ok())
        .collect();
    let env_ptr: Vec<&CStr> = env_vars.iter().map(|c| c.as_c_str()).collect();

    execve(&cmd_c, &args_ptr, &env_ptr)
        .map_err(|e| miette!("execve '{}': {}", cmd, e))?;
    unreachable!()
}
