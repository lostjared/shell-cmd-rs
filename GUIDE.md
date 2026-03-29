# shell-cmd-rs — Complete Guide

## What Is shell-cmd-rs?

`shell-cmd-rs` is a command-line utility written in Rust that **recursively walks a directory tree**, finds files matching a **regex pattern**, and **executes a shell command for each match**. It is a drop-in replacement for the original C++20 `shell-cmd`, offering the same CLI interface with the safety and performance benefits of Rust.

Think of it as a more ergonomic alternative to chaining `find` with `-exec` — you write a command template with numbered placeholders (`%0`, `%1`, `%2`, …) and `shell-cmd-rs` fills them in and runs the command for every matched file.

---

## How the Program Works (Internals)

### High-Level Flow

```
1. Parse command-line options and positional arguments (via clap)
2. Compile the regex pattern and optional exclude pattern
   - In --regex-match mode, anchor patterns with ^(?:...)$ for full-path matching
   - In --glob mode, convert wildcard patterns to regex first
3. If --list-all mode:
   a. Recursively walk the target directory, collecting all matching paths
   b. Join paths into a single string and invoke the command template once with %0 = all matches
4. Otherwise (default per-file mode):
   a. Recursively walk the target directory
   b. For each file whose full path matches the regex:
      i.  Apply metadata filters (size, time, permissions, ownership, type)
      ii. Substitute placeholders in the command template
      iii. Fork a child process and execute the command via the configured shell
5. Print summary statistics to stderr
```

### Argument Parsing

`shell-cmd-rs` uses the [`clap`](https://crates.io/crates/clap) crate with derive macros for argument parsing. It separates **options** (short like `-n` or long like `--dry-run`) from **positional arguments** collected via `trailing_var_arg`. The positional arguments are, in order:

| Position | Meaning |
|----------|---------|
| 1st | **Path** — the root directory to search |
| 2nd | **Command template** — shell command with `%` placeholders |
| 3rd | **Regex** — pattern matched against the full file path |
| 4th+ | **Extra arguments** — substituted into `%2`, `%3`, etc. |

If fewer than three positional arguments are given, the program prints an error and exits.

### Directory Traversal

The function `add_directory()` walks the directory tree using `std::fs::read_dir()`. Key behaviors:

- **Hidden files/directories** (names starting with `.`) are **skipped by default**. Pass `-a` / `--all` to include them.
- **Depth limiting** — if `-d N` is specified, recursion stops after `N` levels (0 means only the given directory, no subdirectories).
- **Permission errors** — directories that can't be opened produce an error message and exit.
- **Symlinks** — symlink detection uses `symlink_metadata()` to avoid following symlinks when checking type. For non-symlink operations, `metadata()` (which follows symlinks) is used.

For every entry whose full path matches the regex and passes all metadata filters, the program calls `proc_cmd()`.

In **list-all mode** (`-l` / `--list-all`), a separate function `fill_list()` walks the tree and collects all matching paths into a vector instead of executing commands. After the walk completes, the paths are joined with spaces and `proc_cmd()` is called once with `%0` expanded to the full list.

### Placeholder Substitution

Inside `proc_cmd()`, the command template string is scanned and placeholders are replaced:

| Placeholder | Replaced With |
|-------------|---------------|
| `%0` | The **filename only** (no directory path), via `Path::file_name()` |
| `%1` | The **full path** to the matched file |
| `%b` | The **basename without extension**, via `Path::file_stem()` |
| `%e` | The **file extension** (including the dot), via `Path::extension()` |
| `%2`, `%3`, … | The **extra arguments** passed after the regex on the command line |

If the full path for `%1` contains spaces, it is automatically wrapped in double quotes to prevent word-splitting by the shell.

### Command Execution

Commands are executed through a custom `system_cmd()` function that mirrors the C++ original's `System()`. Rather than using Rust's `std::process::Command` (which doesn't provide the same signal semantics), this implementation uses the `nix` crate:

1. **Blocks `SIGCHLD`** via `sigprocmask()` so the parent doesn't get interrupted.
2. **Ignores `SIGINT`/`SIGQUIT`** in the parent via `sigaction()` so Ctrl+C doesn't kill the batch runner.
3. **Forks** a child process via `nix::unistd::fork()`.
4. **Restores default signal handlers** in the child process.
5. Runs the command via `execv("/bin/bash", ["bash", "-c", command])` in the child (or the shell specified by `--shell`).
6. **Waits** for the child via `waitpid()`, handling `EINTR` retries.
7. **Restores** signal masks and handlers in the parent after the child exits.

When **parallel mode** (`-j N`) is active, `proc_cmd()` forks child processes directly and maintains a pool of up to N concurrent workers, bypassing `system_cmd()`. The `wait_for_slot()` and `wait_all()` helpers manage the process pool via a global `CHILD_PIDS` mutex-protected vector.

### Crate Architecture

| Crate | Role |
|-------|------|
| `clap` (derive) | Declarative CLI argument parsing |
| `regex` | Rust-native regex engine (faster than ECMAScript regex, similar semantics) |
| `nix` | Safe wrappers for POSIX `fork`, `execv`, `waitpid`, `sigaction`, `sigprocmask` |
| `libc` | Raw FFI bindings for `getpwuid()` and `getgrgid()` (user/group name lookup) |

---

## Building from Source

### Prerequisites

- Rust 1.70+ (install via [rustup](https://rustup.rs/))

### Debug Build

```bash
cd shell-cmd-rs
cargo build
```

The compiled binary is at `target/debug/shell-cmd-rs`.

### Release Build (optimized)

```bash
cargo build --release
```

The compiled binary is at `target/release/shell-cmd-rs`.

### Install System-Wide

```bash
cargo install --path .
```

This installs to `~/.cargo/bin/shell-cmd-rs`. Ensure `~/.cargo/bin` is in your `PATH`.

### Generate Documentation

```bash
cargo doc --open
```

---

## Usage Synopsis

```
shell-cmd-rs [options] <path> "<command %1 [%2 %3..]>" <regex> [extra_args..]
```

### Options

| Short | Long | Description |
|-------|------|-------------|
| `-n` | `--dry-run` | **Dry-run** — print each command but don't execute it |
| `-v` | `--verbose` | **Verbose** — print each command before executing it |
| `-a` | `--all` | **All files** — include hidden files and directories |
| `-l` | `--list-all` | **List all** — collect all matches and run command once with `%0` = all matched paths |
| `-d N` | `--depth N` | **Max depth** — limit recursion (0 = current directory only) |
| `-s SIZE` | `--size SIZE` | **Size filter** — `+10M` (>10 MB), `-1K` (<1 KB), `4096` (exact bytes). Suffixes: K, M, G |
| `-m DAYS` | `--mtime DAYS` | **Modification time** — `+7` (older than 7 days), `-1` (within last day), `3` (exactly 3 days) |
| `-p MODE` | `--perm MODE` | **Permissions** — octal mode, e.g. `755` |
| `-u USER` | `--user USER` | **Owner** — filter by username |
| `-g GROUP` | `--group GROUP` | **Group** — filter by group name |
| `-t TYPE` | `--type TYPE` | **Type** — `f` (file), `d` (directory), `l` (symlink) |
| `-x REGEX` | `--exclude REGEX` | **Exclude** — skip files/directories matching the regex |
| `-i` | `--glob-exclude` | **Glob exclude** — treat the exclude pattern as a glob instead of regex |
| `-e` | `--stop-on-error` | **Stop on error** — halt on first command failure |
| `-c` | `--confirm` | **Confirm** — prompt yes/no before each command |
| `-j N` | `--jobs N` | **Parallel** — run N commands concurrently (default: 1) |
| `-w SHELL` | `--shell SHELL` | **Shell** — shell to use for execution (default: `/bin/bash`) |
| `-b` | `--glob` | **Glob mode** — treat pattern as a glob (`*`, `?`) instead of regex |
| `-z` | `--regex-match` | **Regex match** — entire path must match the regex (anchored with `^(?:...)$`) |
| `-h` | `--help` | **Help** — show usage information |
| `-V` | `--version` | **Version** — print version number |

---

## Concrete Real-Life Examples

### 1. Count Lines of Code in a Project

```bash
shell-cmd-rs . "wc -l %1" ".*\.py$"
```

**What happens:** Every `.py` file found recursively under `.` is passed to `wc -l`. Output looks like:

```
  42 ./src/main.py
 118 ./src/utils.py
  27 ./tests/test_main.py
```

### 2. Preview Before You Act (Dry-Run)

```bash
shell-cmd-rs -n ./src "rustfmt %1" ".*\.rs$"
```

Output (nothing is executed):

```
rustfmt ./src/main.rs
rustfmt ./src/lib.rs
rustfmt ./src/utils.rs

Summary: 3 matched, 3 run, 0 failed
```

When satisfied, remove `-n` to actually format the files.

### 3. Batch Resize Photos

```bash
shell-cmd-rs ~/Photos "convert %1 -resize 1920x1080 /tmp/resized/%0" ".*\.jpe?g$"
```

- `%1` → `/home/you/Photos/vacation/sunset.jpg` (the source)
- `%0` → `sunset.jpg` (used to name the output file)

### 4. Back Up Log Files to Another Directory

```bash
shell-cmd-rs /var/log "cp %1 %2/%0" ".*\.log$" /mnt/backup/logs
```

- `%1` → full path to each log file
- `%2` → `/mnt/backup/logs` (the extra argument)
- `%0` → the filename only

### 5. Search for TODO Comments Across a Codebase

```bash
shell-cmd-rs . "grep -Hn 'TODO' %1" ".*\.(rs|py|ts|cpp)$"
```

### 6. Convert All Markdown Files to PDF

```bash
shell-cmd-rs ~/docs "pandoc %1 -o /tmp/pdfs/%b.pdf" ".*\.md$"
```

Using `%b` gives the basename without extension, so `report.md` produces `report.pdf`.

### 7. Transcode WAV Audio to MP3

```bash
shell-cmd-rs ~/recordings "ffmpeg -i %1 -b:a 192k /tmp/mp3/%b.mp3" ".*\.wav$"
```

### 8. Validate Shell Scripts Without Running Them

```bash
shell-cmd-rs -v . "bash -n %1" ".*\.sh$"
```

### 9. Extract All tar.gz Archives

```bash
shell-cmd-rs ~/Downloads "tar xzf %1 -C /tmp/extracted" ".*\.tar\.gz$"
```

### 10. Strip EXIF Metadata Before Sharing Photos

```bash
shell-cmd-rs ./photos "exiftool -all= %1" ".*\.(jpg|png)$"
```

### 11. Work Only in the Current Directory (No Recursion)

```bash
shell-cmd-rs -d 0 . "cat %1" ".*\.txt$"
```

### 12. Include Hidden Config Files

```bash
shell-cmd-rs -a ~ "cat %1" ".*\.bashrc|.*\.zshrc"
```

### 13. Combine Multiple Options

```bash
shell-cmd-rs -n -a -d 2 ~ "wc -l %1" ".*rc$"
```

### 14. Using Multiple Extra Arguments

```bash
shell-cmd-rs . "cp %1 %2/%0 && echo 'copied to %3'" ".*\.conf$" /backup user@host
```

- `%2` → `/backup`
- `%3` → `user@host`

The program validates that every extra argument has a corresponding placeholder. Missing placeholders cause an error.

---

## Metadata Filter Examples

### 15. Find Large Files

```bash
shell-cmd-rs . "ls -lh %1" ".*" --size +10M
```

### 16. Delete Old Temp Files (Dry-Run)

```bash
shell-cmd-rs --dry-run /tmp "rm %1" ".*\.tmp$" --mtime +30
```

### 17. Find Executable Files

```bash
shell-cmd-rs . "echo %1" ".*" --perm 755 --type f
```

### 18. List Files Owned by a User

```bash
shell-cmd-rs /etc "echo %1" ".*\.conf$" --user root
```

### 19. Find Files by Group

```bash
shell-cmd-rs /var/www "echo %1" ".*" --group www-data
```

### 20. List Only Directories

```bash
shell-cmd-rs . "echo %1" ".*src.*" --type d
```

### 21. Find Symlinks

```bash
shell-cmd-rs /usr/local "ls -la %1" ".*" --type l
```

### 22. Combine Multiple Filters

Find large `.log` files modified in the last 7 days, owned by `syslog`:

```bash
shell-cmd-rs /var/log "wc -l %1" ".*\.log$" -s +1M -m -7 -u syslog
```

---

## New in v1.2

### 23. Exclude Patterns

Skip `node_modules` and `.git` directories when counting TypeScript lines:

```bash
shell-cmd-rs -x "node_modules|\.git" . "wc -l %1" ".*\.ts$"
```

### 24. Glob Mode

Use `--glob` / `-b` to write familiar wildcard patterns instead of regex. `*` matches anything, `?` matches a single character, and special regex characters (`.`, `+`, `(`, etc.) are auto-escaped:

```bash
shell-cmd-rs --glob . "echo %1" "*.rs"
```

Glob also applies to `--exclude` when combined with `--glob-exclude` / `-i`:

```bash
shell-cmd-rs --glob -x "*.o" --glob-exclude . "echo %1" "*.c"
```

Without `--glob-exclude`, the `-x` pattern is always treated as a regex:

```bash
shell-cmd-rs --glob -x "build|CMakeFiles" . "echo %1" "*.rs"
```

### 25. Regex Match Mode

By default, the regex is tested as a **substring search** — if the pattern appears anywhere in the full path, it matches. Use `-z` / `--regex-match` to require the **entire path** to match the pattern:

```bash
# Default (regex-search): matches any path containing ".rs"
shell-cmd-rs . "echo %1" "\.rs"

# Regex-match: the entire path must match the pattern
shell-cmd-rs -z . "echo %1" ".*\.rs$"
```

This is equivalent to the C++ version’s `std::regex_match` vs `std::regex_search`. Under the hood, `--regex-match` wraps the pattern with `^(?:...)$`.

Combine with `--glob` for anchored wildcard matching:

```bash
shell-cmd-rs -z --glob . "echo %1" "*.rs"
```

### 26. Basename & Extension Placeholders

Convert WAV audio files to MP3, using `%b` to name the output file without the original extension:

```bash
shell-cmd-rs ~/music "ffmpeg -i %1 /tmp/mp3/%b.mp3" ".*\.wav$"
```

Extract extensions to organize files by type:

```bash
shell-cmd-rs -n . "mkdir -p /tmp/by-ext/%e && cp %1 /tmp/by-ext/%e/%0" ".*"
```

### 27. Parallel Execution

Resize images using 4 parallel jobs:

```bash
shell-cmd-rs -j 4 ./images "convert %1 -resize 800x600 /tmp/thumbs/%0" ".*\.jpg$"
```

### 28. Confirm Mode

Interactively confirm before each destructive action:

```bash
shell-cmd-rs -c /tmp "rm %1" ".*\.bak$"
```

Output:

```
Execute: rm /tmp/old.bak ? [y/N]
```

### 29. Stop on Error

Compile all C files and stop at the first failure:

```bash
shell-cmd-rs -e ./src "gcc -c %1 -o /tmp/%b.o" ".*\.c$"
```

### 30. Summary Statistics

A summary line is automatically printed to stderr when verbose, dry-run, or any command failed:

```bash
shell-cmd-rs -v . "wc -l %1" ".*\.py$"
```

```
Summary: 12 matched, 12 run, 0 failed
```

### 31. List-All Mode

Collect all matching file paths and pass them to a single command invocation:

```bash
shell-cmd-rs -l . "cat %0" ".*\.txt$"
```

This finds every `.txt` file recursively and runs `cat` once with all paths as arguments — equivalent to `cat file1.txt file2.txt file3.txt ...`.

Dry-run to preview the combined command:

```bash
shell-cmd-rs -l -n . "wc -l %0" ".*\.rs$"
```

Output:

```
wc -l ./src/main.rs ./src/lib.rs ./src/utils.rs
```

Combine with other filters:

```bash
shell-cmd-rs -l -s +1K . "tar czf archive.tar.gz %0" ".*\.log$"
```

This collects all `.log` files larger than 1 KB and creates a single tar archive containing all of them.

---

## Placeholder Quick Reference

| Placeholder | Value |
|-------------|-------|
| `%0` | Filename only (e.g., `report.txt`); in `--list-all` mode, all matched paths |
| `%1` | Full path (e.g., `/home/user/docs/report.txt`) |
| `%b` | Basename without extension (e.g., `report`) |
| `%e` | File extension with dot (e.g., `.txt`) |
| `%2` | First extra argument after the regex |
| `%3` | Second extra argument after the regex |
| `%N` | Nth extra argument (no upper limit) |

---

## shell-cmd-rs vs `find -exec`

| Feature | `shell-cmd-rs` | `find -exec` |
|---------|----------------|---------------|
| **Filename placeholder** | `%0` gives the filename without the path | No equivalent — requires `sh -c` + `basename` |
| **Full path placeholder** | `%1` | `{}` |
| **Extra arguments** | `%2`, `%3`, … with validation | Not supported — use shell variables |
| **Pattern matching** | Rust `regex` crate on the full path; `--regex-match` for full-path anchoring; `--glob` for wildcard patterns | Glob (`-name`) or implementation-varying `-regex` |
| **Dry-run** | Built-in `-n` flag | No native support |
| **Verbose mode** | Built-in `-v` flag | No native support |
| **Filtering by metadata** | Size, time, permissions, owner, group, type | Size, time, permissions, ownership, type, boolean logic |
| **Exclude patterns** | Built-in `-x` / `--exclude` with regex | Requires negation logic or `! -name` |
| **Parallel execution** | Built-in `-j N` / `--jobs N` | Requires `xargs -P` or GNU `parallel` |
| **List-all mode** | Built-in `-l` / `--list-all` — run command once with all matches | Requires `xargs` or `+` terminator |
| **Confirm mode** | Built-in `-c` / `--confirm` | Requires `-ok` (not universally supported) |
| **Stop on error** | Built-in `-e` / `--stop-on-error` | No native support |
| **Summary statistics** | Automatic (matched/run/failed counts) | No native support |
| **Portability** | Requires Rust toolchain | POSIX-standard, available everywhere |

### Side-by-Side Examples

**Copy all `.txt` files to a backup directory, preserving filenames:**

```bash
# shell-cmd-rs
shell-cmd-rs . "cp %1 /tmp/backup/%0" ".*\.txt$"

# find equivalent (needs sh -c + basename gymnastics)
find . -regex '.*\.txt$' -exec sh -c 'cp "$1" "/tmp/backup/$(basename "$1")"' _ {} \;
```

**Dry-run to preview commands:**

```bash
# shell-cmd-rs — built-in
shell-cmd-rs -n . "rm %1" ".*\.bak$"

# find — no native dry-run, must rework the command
find . -regex '.*\.bak$' -exec echo rm {} \;
```

### When to Use Which

- **Use `shell-cmd-rs`** when your command needs the filename separated from the path, when you want to inject extra arguments, or when you want built-in dry-run/verbose, parallel execution, confirm mode, exclude patterns, stop-on-error, and summary statistics.
- **Use `find`** when you need boolean logic combining filters (`-and`, `-or`, `-not`) — or when you're on a system without a Rust toolchain.

---

## Differences from the C++ Original

`shell-cmd-rs` is a **100% compatible drop-in replacement**. The notable implementation differences are:

| Aspect | C++ `shell-cmd` | Rust `shell-cmd-rs` |
|--------|------------------|---------------------|
| Argument parser | Custom `argz.hpp` header | `clap` derive macros |
| Regex engine | `std::regex` (ECMAScript) | `regex` crate (similar syntax, faster) |
| Filesystem | `std::filesystem` | `std::fs` |
| Process mgmt | Manual `fork`/`execl`/`waitpid` | `nix` crate wrappers around same syscalls |
| User/group lookup | `getpwuid`/`getgrgid` | `libc` FFI to same POSIX calls |
| Memory safety | Manual | Guaranteed by Rust's ownership model |
| Build system | CMake / Makefile | Cargo |

---

## Tips and Best Practices

1. **Always dry-run first.** Use `-n` before running destructive commands (`rm`, `mv`, overwriting files) to verify what will execute.

2. **Quote the command template.** Since it contains `%` placeholders and often shell metacharacters, always wrap it in double quotes: `"command %1"`.

3. **Escape regex special characters.** To match a literal dot in file extensions, use `\.` — e.g., `".*\.rs$"` not `".*rs$"` (the latter also matches `ars`). Alternatively, use `--glob` to avoid regex escaping altogether: `--glob "*.rs"`.

4. **Understand the two regex modes.** By default, the regex matches anywhere in the path (substring search). Use `-z` / `--regex-match` when you need the entire path to match — this is stricter and requires patterns like `".*\.rs$"` instead of just `"\.rs"`.

4. **Use `%0` for output filenames.** When copying/converting files to a new directory, `%0` gives you the original filename without the source path.

5. **Use `%b` for format conversion.** When converting between formats (e.g., WAV→MP3), `%b` gives the stem without extension, perfect for naming the output.

6. **Combine with `--release` builds.** For large directory trees, use `cargo build --release` for optimal traversal speed.
