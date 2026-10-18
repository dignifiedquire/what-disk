# What Disk

A fast disk usage analyzer for macOS, written in Rust with a [Dioxus](https://dioxuslabs.com)
interface that looks and behaves like a native Mac app.

## Features

- **Scan** a volume, your home folder, or any folder. Scanning runs in parallel and shows
  results while it runs. Sizes are the space files take on disk, hard links count once,
  and other volumes are skipped.
- **Browse** the results in Finder-style columns next to a sunburst chart, or list the
  biggest files, or search by name.
- **Remove** files and folders: check them and choose Review and Remove, or right-click.
  Items go to the Trash, or are deleted immediately after a confirmation. Progress shows
  in the sheet or the bottom bar, and each item leaves the views as soon as it is gone.
- **Keep results**: every scan is saved and shown instantly the next time. Rescan a whole
  location or a single folder when you want fresh numbers.
- **Pause and resume** a scan from the toolbar. A running scan saves its progress every two
  minutes and when you quit, so an unfinished scan continues where it stopped, even after a
  restart.
- **Projects & Toolchains** lists the build folders of Rust (`target/`), JavaScript
  (`node_modules/`), Java (Maven `target/`, Gradle `build/` and `.gradle/`), C/C++ (CMake
  and Meson build folders), Swift (`.build/`), Python (virtual environments, `.tox`,
  `.nox` and tool caches) and Ruby (`vendor/bundle`, `tmp/cache`) projects, plus toolchain
  caches and versions across all saved scans. Double-click a folder, or right-click and
  choose Show in, to see what is inside it in the scan that holds it:
  - Rust: Cargo, rustup, kache, sccache
  - JavaScript: nvm, Deno, npm, pnpm, Yarn, Bun
  - Go: module cache, build cache, golangci-lint, SDKs in `~/sdk`
  - Java: Maven repository, Gradle caches and distributions, JDKs
  - C/C++: ccache, Conan, vcpkg
  - Swift: Xcode DerivedData per project, device support per OS version, simulator and
    SwiftPM caches, Swift toolchains
  - Python: pip, uv, pipx, Poetry, Pipenv, PDM and Hatch caches; Pythons from uv and
    pyenv; Poetry, Pipenv and Hatch environments
  - Ruby: gems per Ruby version, RubyGems and Bundler caches, Rubies from rbenv, rvm and
    ruby-install
  - asdf and mise installs, filed under their language
  - AI: Claude Code transcripts, checkpoints and logs (including MCP logs), Claude desktop's
    VM, Codex, Gemini CLI, Copilot CLI, opencode, Aider and Continue data, and models from
    Ollama, LM Studio, Hugging Face (one entry per model), PyTorch, Whisper, llama.cpp and
    Jan. Each entry says what deleting it costs.

  Locations come from each tool's documentation, adjusted by your shell environment (for
  example `CARGO_HOME` or `XDG_CACHE_HOME` set in `.zshrc`) and by asking installed tools
  such as `go env` or `npm config get cache`.
- **Apps** lists applications and Homebrew packages with the space they and their data in
  `~/Library` use, and can uninstall them.

## Requirements

- macOS (Apple silicon or Intel)
- Rust 1.88 or newer

## Build and run

```sh
cargo run --release
```

Run the tests with `cargo test`.

## Permissions

macOS protects some folders, such as Mail, Safari and other apps' containers. Without Full
Disk Access, What Disk skips them and shows their space as unaccounted. When you start What
Disk with `cargo run`, it uses your terminal app's permissions, so give the terminal app
Full Disk Access in System Settings > Privacy & Security > Full Disk Access.

## Data

Each scan is written to disk while it runs, in `~/Library/Caches/com.dignifiedquire.whatdisk`:
one file per location, holding every folder's listing as it is read. Memory keeps only a
small entry per folder (a whole startup disk with 12 million items needs about 60 MB);
views read file and folder details back from the file. Searching or listing the biggest
files of a whole disk therefore reads its file, which takes a second or two.

Deleting that folder only means the next start shows no previous scans. Scans saved by
earlier versions are converted the first time they are opened.

## Development

Debug builds read commands from standard input so the interface can be driven from
scripts: each line is run as JavaScript in the page and its result is printed,
`:open <path>` opens a folder, `:locations` prints where the tools said they keep their
data, and `:quit` and `:close` quit the app like ⌘Q and closing the window. Release builds
do not include this.

| Module | Purpose |
| --- | --- |
| `src/scan.rs` | Parallel, resumable scanning with `dua-core` |
| `src/store.rs` | A scan on disk: writing listings, reading them back, saved scans |
| `src/volumes.rs` | Volumes and which folders a scan may enter |
| `src/dev.rs` | Finding project build folders and toolchain caches |
| `src/apps.rs`, `src/brew.rs` | Applications, their data, and Homebrew |
| `src/ops.rs` | Trash, delete, Finder, Quick Look and icons |
| `src/app/` | The user interface |
