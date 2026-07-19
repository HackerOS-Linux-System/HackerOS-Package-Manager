# HPM — Hacker Package Manager

HPM is the community package manager for HackerOS. It installs, builds, sandboxes,
and manages packages entirely in **your own home directory** — no root required
for day-to-day use.

## What's new in 0.9

- **No more root.** Packages install to `~/.hackeros/hpm/store/`, wrappers to
  `~/.local/bin/`, desktop/icon integration to `~/.local/share/...`. Old versions
  used `/usr/lib/HackerOS/hpm/store/` + `/usr/bin/`, which required `sudo` for
  every operation. That's gone — see [Directory layout](#directory-layout).
- **`hpm dev <path>`** — test a local package directory that isn't (yet) in
  `repo-list.json` / `repo.json`. See [Local package testing](#local-package-testing-hpm-dev).
- **`hpm install <pkg> --release`** — install from a package's GitHub Release
  `.hpm` asset instead of `git clone`-ing the whole repo. See
  [Distributing via GitHub Releases](#distributing-via-github-releases---release).
- **Hooks now run sandboxed** (namespaces + seccomp + Landlock + rlimits), and
  `pre-remove` / `post-remove` / `post-update` hooks actually fire now (they
  used to be defined but never called).
- **Preflight conflict checking** — `hpm install a b c` resolves and cross-checks
  every package's manifest *before* touching any files, instead of discovering a
  conflict halfway through a multi-package install.
- **`hpm build`** can now sign packages (`--sign <key-id>`, GPG detached
  signature) and always emits a `.sha256` checksum alongside the `.hpm` archive —
  the same integrity story as pacman's `.pkg.tar.zst` + `.sig`.

## Install

```sh
hpm install <package>[@<version>]
hpm install <package> --release      # from GitHub Releases instead of git clone
hpm install @<tag>                   # install every package with a given group tag
```

No `sudo` needed. If you see a permission error, it almost always means
`~/.hackeros` or `~/.local/bin` was previously created by root (e.g. a stray
`sudo hpm ...` before 0.9) — `hpm` will tell you exactly what to `chown`.

## Commands

### Package management
| Command | Description |
|---|---|
| `hpm install <pkg>[@<ver>]...` | Install one or more packages |
| `hpm install <pkg> --release` | Install from a GitHub Release `.hpm` asset |
| `hpm install @<tag>` | Install every package tagged `<tag>` |
| `hpm remove <pkg>[@<ver>]` | Remove a package (runs pre/post-remove hooks) |
| `hpm autoremove` | Remove orphaned dependencies |
| `hpm update` | Update all packages (runs post-update hooks) |
| `hpm switch <pkg> <ver>` | Switch the active version |
| `hpm rollback [<pkg>]` | Restore previous state |
| `hpm pin` / `hpm unpin` | Lock a package to a version across updates |

### Query
| Command | Description |
|---|---|
| `hpm search <query\|@tag>` | Search the package index |
| `hpm info <package>` | Show package details |
| `hpm list` | List installed packages |
| `hpm outdated` | Show packages with available updates |
| `hpm deps <pkg>[@<ver>]` | Show dependency tree |
| `hpm diff <pkg> <v1> [<v2>]` | Compare two package versions |

### Running & building
| Command | Description |
|---|---|
| `hpm run <pkg> <bin> [args]` | Run an installed binary, sandboxed |
| `hpm build [name]` | Package the current directory into a `.hpm` archive |
| `hpm build --output <path>` | Custom output path |
| `hpm build --sign <key-id>` | Also produce a detached GPG `.sig` |
| `hpm verify <package>` | Verify SHA-256 + GPG signature |
| `hpm dev <path>` | Test a local package directory (see below) |
| `hpm dev <path> run <bin> [args]` | ...and run one of its binaries |

### Maintenance
| Command | Description |
|---|---|
| `hpm doctor` | Diagnose consistency issues |
| `hpm repair` | Auto-fix issues found by `doctor` |
| `hpm clean [--all]` | Remove cached repos/temp files (and old versions with `--all`) |
| `hpm lock <subcmd>` | Manage `hpm.lock` for reproducible installs |

Run `hpm --help` for the full, current list — this table is a summary, the
`--help` output is generated from the same code that implements the commands
so it can't drift out of sync the way this file can.

## Directory layout

```
~/.hackeros/hpm/
├── store/     installed packages (was /usr/lib/HackerOS/hpm/store/)
├── cache/     repo index (repo-list.json/repo.json) + downloaded .hpm files
└── db/        state.json, wrapper-names.json, system.lock

~/.local/
├── bin/                       binary wrappers (was /usr/bin/)
├── share/applications/        .desktop files (was /usr/share/applications/)
├── share/icons/hicolor/       icons (was /usr/share/icons/hicolor/)
├── share/pixmaps/
├── share/bash-completion/completions/
├── share/zsh/site-functions/
└── config/fish/completions/
```

Everything above is per-user. Two packages installed by two different users on
the same machine don't conflict, and nothing here needs elevated privileges.

The one exception is `hpm upgrade` (upgrading **hpm itself**, not a package it
manages) — that still touches the system-wide `hpm` binary and may ask for
`sudo`, same as upgrading any other system tool.

## Local package testing: `hpm dev`

`hpm dev` has two modes. With no path, it runs hpm's own internal test suite
(`hpm dev test` / `hpm dev test-full` / `hpm dev check-env`) — useful if you're
hacking on hpm itself.

With a path, it tests **your** package:

```sh
hpm dev ./my-package
hpm dev /home/user/packages/firefox
```

The directory needs the same layout a real package would (`info.hk`, plus
`contents/` or a build step) — it does **not** need to be listed in
`repo-list.json` / `repo.json`. This is the intended workflow for porting a new
tool: build the package directory, `hpm dev` it repeatedly while iterating, and
only register it in the repo index once it works.

`hpm dev <path>` will:
1. Load and print the manifest (name, version, arch, sandbox policy, hooks).
2. Validate hooks and check architecture compatibility.
3. Build `contents/` if it's missing (`build.toml` first, then classic
   `[build] commands` / `build.info` — same code path `hpm install` uses).
4. Run `pre-install` and `post-install` hooks, sandboxed, exactly like a real
   install (without touching `~/.hackeros/hpm/store`).
5. List every declared binary and whether it was actually found.

Then run one of its binaries through the exact same sandbox `hpm run` uses:

```sh
hpm dev ./my-package run my-binary --some-flag
```

## Distributing via GitHub Releases (`--release`)

`repo-list.json` / `repo.json` still just contains a repo URL — that format is
unchanged:

```json
{ "packages": { "my-tool": "https://github.com/someone/my-tool" } }
```

By default `hpm install my-tool` clones that repo with git. If you instead run

```sh
hpm build --output my-tool.hpm --sign <your-gpg-key-id>
```

in your package directory and attach `my-tool.hpm` (+ the `.sha256` it
produces, + the `.sig` if you signed it) to a GitHub Release, then anyone can
fetch that exact artifact instead of cloning your repo:

```sh
hpm install my-tool --release          # latest release
hpm install my-tool@v1.2.0 --release   # specific tag
```

hpm verifies the `.sha256` automatically if it's present next to the `.hpm`
asset. This doesn't change how packages are registered — it's purely a faster,
optionally-signed transport for packages that opt in by publishing releases.

## Hooks

Package hooks live in `hooks/` and run **sandboxed** (namespaces, seccomp,
Landlock, resource limits — the same isolation `hpm run` uses for the app
itself):

| Hook | Runs | Blocks install/remove on failure? |
|---|---|---|
| `pre-install` | before install | yes |
| `post-install` | after install | no |
| `pre-remove` | before removal | yes |
| `post-remove` | after removal | no |
| `post-update` | after update | no |

Hooks never get network access by default, even if the package itself declares
`sandbox.network = true` — that only applies to the app, not its install-time
scripts. If a hook genuinely needs network (e.g. registering with an external
service on first install), declare it explicitly and narrowly:

```
[sandbox]
-> hooks_network => true
```

Supported hook interpreters, by extension: `.hl` (Hacker Lang), `.py`, `.rb`,
`.sh`.

## Package format

```
info.hk          Manifest: name, version, bins, deps, conflicts, sandbox, tags
build.toml        Build/download instructions (optional — see BuildConfig)
contents/         Pre-built files (skip build.toml if you ship these directly)
hooks/            pre-install / post-install / pre-remove / post-remove / post-update
info.hk.sig       GPG signature (optional)
```

## Security model

- Every package runs sandboxed by default (Linux namespaces + seccomp +
  Landlock + resource limits), degrading gracefully layer-by-layer when the
  host/container doesn't support a given primitive rather than refusing to run
  at all.
- Hooks get the same sandboxing, network-isolated by default (see above).
- `hpm install` resolves and conflict-checks every package in a batch *before*
  installing any of them.
- `.hpm` archives fetched via `--release` are checksum-verified when a
  `.sha256` asset is published alongside them, and can be GPG-signed.

## Known limitations (being worked on)

- No full dependency SAT solver yet — conflicts are checked, but hpm won't
  automatically pick alternate versions to resolve them.
- State/history is still a JSON file under `~/.hackeros/hpm/db/`, not a real
  embedded database.
- `hpm upgrade` (upgrading hpm itself) still uses system paths and may need
  `sudo` — everything else doesn't.
