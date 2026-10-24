# Install

## From source

```sh
git clone git@github.com:marcosgabrielgomes110-collab/Thais.git
cd Thais
make
make install PREFIX="$HOME/.local"
```

`make install` installs `thais` and the Linux `thais-sandbox` helper.

## Docker

```sh
docker build -t thais .
docker run --rm -it -v "$PWD:/work" -w /work thais run --task "hello"
```

## Release artifacts

`dist-workspace.toml` defines cargo-dist targets for Linux, macOS, and Windows,
plus shell, Homebrew, PowerShell, and MSI installers. Release artifacts should
include checksums and an SBOM; signing is part of the stable release gate.

## First task

```sh
make
cargo run -p thais-cli -- tui --provider fake
```
