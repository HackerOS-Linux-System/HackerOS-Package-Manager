# hpm-example-repo

Minimal example package for the [HackerOS Package Manager (hpm)](https://github.com/HackerOS-Linux-System/HackerOS-Package-Manager).

## Repository layout

```
hpm-example-repo/
├── info.hk              ← required: package manifest
├── contents/            ← required: files installed to the store
│   └── bin/
│       └── hello-hpm   ← the actual binary (chmod +x)
└── README.md
```

## info.hk quick reference

```
[metadata]
-> name => <package-name>
-> version => <semver>
-> authors => <author>
-> license => <spdx>
-> bins.<binary-name> => ""   ← each binary that gets a /usr/bin wrapper

[description]
-> summary => One-line description shown in hpm search
-> long => Longer description shown in hpm info

[sandbox]
-> network => false   ← allow network inside sandbox?
-> gui => false       ← allow X11/Wayland?
-> dev => false       ← expose /dev devices?
-> full_gui => false  ← full desktop access?
-> filesystem => {}   ← extra host paths to bind-mount (read-write)

[build]
-> commands => {}     ← shell commands run before install (if no build.toml)
-> deb_deps => {}     ← Debian packages needed at build time

[runtime]
-> deb_deps => {}     ← Debian packages needed at runtime
```

## Optional: build.toml

Use `build.toml` when the binary needs to be downloaded or compiled.  
When `build.toml` is absent, hpm uses `contents/` directly.

### Download a pre-built binary from GitHub Releases

```toml
type = "download"
url = "https://github.com/user/repo/releases/download/v{version}/mybinary-linux-x86_64"
install_path = "bin/mybinary"
```

### Build from source

```toml
type = "build"
commands = ["cargo build --release"]
output   = "target/release/mybinary"
install_path = "bin/mybinary"
build_deps = ["build-essential"]
```

## Tagging versions

hpm reads versions from git tags. Tag your releases as `v1.0.0` or `1.0.0`:

```sh
git tag v1.0.0
git push origin v1.0.0
```

## Adding to repo.json

```json
{
  "packages": {
    "hello-hpm": {
      "repo": "https://github.com/yourname/hpm-example-repo"
    }
  }
}
```

The `versions` field is optional — hpm discovers them from git tags automatically.
