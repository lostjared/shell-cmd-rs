# shell-cmd-rs

Recursively find files matching a regex pattern and execute a shell command for each match.
Drop-in replacement for [shell-cmd](https://github.com/lostjared/shell-cmd/), rewritten in Rust.

## Build

Requires Rust 1.70+ (for `LazyLock` stabilization).

```bash
cargo build --release
```

The compiled binary is at `target/release/shell-cmd-rs`.

### Install via Cargo

```bash
cargo install --path .
```

## Usage

```
shell-cmd-rs [options] path "command %1 [%2 %3..]" regex [extra_args..]
```

### Placeholders

| Placeholder | Description |
|-------------|-------------|
| `%0` | Filename only (no path); in `--list-all` mode, all matched paths joined by spaces |
| `%1` | Full path to matched file |
| `%2+` | Extra arguments from command line |
| `%b` | Basename without extension (e.g., `report` from `report.txt`) |
| `%e` | File extension including dot (e.g., `.txt`) |

### Options

| Short | Long | Description |
|-------|------|-------------|
| `-n` | `--dry-run` | Dry-run — print commands without executing |
| `-v` | `--verbose` | Verbose — print each command before running |
| `-a` | `--all` | Include hidden files and directories |
| `-l` | `--list-all` | Collect all matches and run command once with `%0` = all matched paths |
| `-d N` | `--depth N` | Max recursion depth (0 = current directory only) |
| `-s SIZE` | `--size SIZE` | Filter by size: `+10M` (>10 MB), `-1K` (<1 KB), `4096` (exact). Suffixes: K, M, G |
| `-m DAYS` | `--mtime DAYS` | Filter by modification time: `+7` (older than 7 days), `-1` (within last day) |
| `-p MODE` | `--perm MODE` | Filter by permissions (octal), e.g. `755` |
| `-u USER` | `--user USER` | Filter by owner username |
| `-g GROUP` | `--group GROUP` | Filter by group name |
| `-t TYPE` | `--type TYPE` | Filter by type: `f` (file), `d` (directory), `l` (symlink) |
| `-x REGEX` | `--exclude REGEX` | Exclude files/directories matching REGEX |
| `-e` | `--stop-on-error` | Stop on first command failure |
| `-c` | `--confirm` | Prompt for confirmation before each command |
| `-j N` | `--jobs N` | Run N commands in parallel (default: 1) |
| `-w SHELL` | `--shell SHELL` | Shell to use for execution (default: `/bin/bash`) |
| `-h` | `--help` | Show help |

## Examples

Count lines in all `.rs` files:

```bash
shell-cmd-rs . "wc -l %1" ".*\.rs$"
```

Dry-run to preview what would be executed:

```bash
shell-cmd-rs -n . "rustfmt %1" ".*\.rs$"
```

Copy matched files to a destination, using filename-only placeholder:

```bash
shell-cmd-rs . "cp %1 /tmp/backup/%0" ".*\.txt$"
```

Limit search to current directory (no recursion):

```bash
shell-cmd-rs -d 0 . "cat %1" ".*\.md$"
```

Use extra arguments — `%2` is replaced with the value passed after the regex:

```bash
shell-cmd-rs . "cp %1 %2/%0" ".*\.log$" /tmp/logs
```

Include hidden files:

```bash
shell-cmd-rs -a ~ "echo %1" ".*\.bashrc"
```

Find large files (over 10 MB):

```bash
shell-cmd-rs . "ls -lh %1" ".*" --size +10M
```

Delete files older than 30 days, with dry-run:

```bash
shell-cmd-rs --dry-run /tmp "rm %1" ".*\.tmp$" --mtime +30
```

Find executable files (permission 755):

```bash
shell-cmd-rs . "echo %1" ".*" --perm 755 --type f
```

List files owned by root:

```bash
shell-cmd-rs /etc "echo %1" ".*\.conf$" --user root
```

List only directories matching a pattern:

```bash
shell-cmd-rs . "echo %1" ".*src.*" --type d
```

Combine filters — large `.log` files modified recently:

```bash
shell-cmd-rs /var/log "wc -l %1" ".*\.log$" -s +1M -m -7
```

Exclude `node_modules` and `.git` directories:

```bash
shell-cmd-rs -x "node_modules|\.git" . "wc -l %1" ".*\.ts$"
```

Convert WAV to MP3, using `%b` for the output filename without extension:

```bash
shell-cmd-rs ~/music "ffmpeg -i %1 /tmp/mp3/%b.mp3" ".*\.wav$"
```

Run commands in parallel with 4 jobs:

```bash
shell-cmd-rs -j 4 ./images "convert %1 -resize 800x600 /tmp/thumbs/%0" ".*\.jpg$"
```

Confirm before each destructive command:

```bash
shell-cmd-rs -c /tmp "rm %1" ".*\.bak$"
```

Stop on first error:

```bash
shell-cmd-rs -e ./src "gcc -c %1 -o /tmp/%b.o" ".*\.c$"
```

Collect all matches and pass them as a single argument list:

```bash
shell-cmd-rs -l . "cat %0" ".*\.txt$"
```

Dry-run list-all mode to preview the combined command:

```bash
shell-cmd-rs -l -n . "wc -l %0" ".*\.rs$"
```

## How It Works

The program recursively walks the specified directory using Rust's `std::fs`. For each file whose path matches the given regex (via the `regex` crate), it substitutes placeholders in the command template and executes it via `fork`/`execv` through the configured shell (`/bin/bash` by default). Hidden files and directories are skipped by default.

Command execution uses the `nix` crate for POSIX `fork`, `execv`, `waitpid`, and signal management — matching the behavior of the original C++ implementation's custom `System()` function.

## Dependencies

| Crate | Purpose |
|-------|---------|
| [`clap`](https://crates.io/crates/clap) | Command-line argument parsing with derive macros |
| [`regex`](https://crates.io/crates/regex) | Fast regex matching for file path filtering |
| [`nix`](https://crates.io/crates/nix) | POSIX APIs: `fork`, `execv`, `waitpid`, signal handling |
| [`libc`](https://crates.io/crates/libc) | Raw FFI for `getpwuid`/`getgrgid` (user/group lookup) |

## shell-cmd-rs vs `find -exec`

| Feature | `shell-cmd-rs` | `find -exec` |
|---------|----------------|---------------|
| **Filename placeholder** | `%0` gives the filename without the path | No equivalent — requires `sh -c` + `basename` |
| **Full path placeholder** | `%1` | `{}` |
| **Extra arguments** | `%2`, `%3`, … with validation | Not supported — use shell variables |
| **Pattern matching** | Rust `regex` crate on the full path | Glob (`-name`) or implementation-varying `-regex` |
| **Dry-run** | Built-in `-n` flag | No native support |
| **Verbose mode** | Built-in `-v` flag | No native support |
| **Filter by metadata** | Size, time, permissions, owner, group, type | Size, time, permissions, ownership, type, boolean logic |
| **Exclude patterns** | Built-in `-x` with regex | Requires negation logic or `! -name` |
| **Parallel execution** | Built-in `-j N` | Requires `xargs -P` or GNU `parallel` |
| **List-all mode** | Built-in `-l` collects matches into single command | Requires `xargs` or `+` terminator |
| **Confirm mode** | Built-in `-c` flag | Requires `-ok` (not universally supported) |
| **Stop on error** | Built-in `-e` flag | No native support |
| **Summary stats** | Automatic (matched/run/failed counts) | No native support |

## Compatibility

`shell-cmd-rs` is a **drop-in replacement** for `shell-cmd`. All command-line flags, positional arguments, placeholder syntax, output format, and exit codes are identical. You can alias it:

```bash
alias shell-cmd='shell-cmd-rs'
```

## License

GNU GPL v3
