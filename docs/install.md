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

`dist-workspace.toml` defines cargo-dist targets for Linux with a shell
installer. Release artifacts include checksums and an SBOM; signing is part of
the stable release gate. See [release.md](release.md). For local cargo-dist artifacts, run
`./scripts/dist-local.sh v0.1.0`.

## First task

```sh
make
cargo run -p thais-cli -- tui --provider fake
```
