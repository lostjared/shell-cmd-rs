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
| `-b` | `--glob` | Treat pattern as a glob (`*`, `?`) instead of regex |
| `-z` | `--regex-match` | Use full-path matching (entire path must match the regex) instead of substring search |
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
| `-i` | `--glob-exclude` | Treat exclude pattern as a glob instead of regex |
| `-f EXPR` | `--expr EXPR` | Expression filter — compose `glob()`, `regex()`, `regex_match()` with `and`/`or`/`not` (replaces the regex positional argument) |
| `-e` | `--stop-on-error` | Stop on first command failure |
| `-c` | `--confirm` | Prompt for confirmation before each command |
| `-j N` | `--jobs N` | Run N commands in parallel (default: 1) |
| `-w SHELL` | `--shell SHELL` | Shell to use for execution (default: `/bin/bash`) |
| `-h` | `--help` | Show help |

---

## Pattern Matching Modes

`shell-cmd-rs` supports three independent switches that control how the search pattern and exclude pattern are interpreted. They can be combined freely.

### Default: Regex Search

By default, the third positional argument is a **regex** tested as a **substring search** against each file's full path. If the pattern appears anywhere in the path, the file matches.

```bash
# Matches any path containing ".rs" — e.g. ./src/main.rs, ./lib/foo.rs
shell-cmd-rs . "echo %1" "\.rs"

# Anchor with $ to match only paths ending in .rs
shell-cmd-rs . "echo %1" "\.rs$"

# Match .c, .cpp, .h, and .hpp files
shell-cmd-rs . "echo %1" "\.(c|cpp|h|hpp)$"
```

Because this is a substring search, you do **not** need `.*` at the start of the pattern — `\.rs$` is enough to match all paths ending in `.rs`.

### `--regex-match` / `-z`: Full-Path Matching

With `-z`, the regex must match the **entire path** (equivalent to wrapping the pattern in `^...$`). This is useful when you want precise control:

```bash
# Only matches paths that are entirely ".*\.rs$"
shell-cmd-rs -z . "echo %1" ".*\.rs$"

# Match files whose full path starts with ./src/ and ends with .rs
shell-cmd-rs -z . "echo %1" "\./src/.*\.rs"
```

### `--glob` / `-b`: Glob Mode

With `-b`, write familiar shell wildcard patterns instead of regex. Glob metacharacters:

| Glob | Meaning | Regex equivalent |
|------|---------|-----------------|
| `*` | Match any number of characters | `.*` |
| `?` | Match exactly one character | `.` |
| `[abc]` | Match one of the listed characters | `[abc]` |
| `[!abc]` or `[^abc]` | Match any character not listed | `[^abc]` |

All other regex-special characters (`.`, `+`, `|`, `(`, `)`, etc.) are automatically escaped, so you never need backslashes.

The glob pattern is anchored — it must match the **entire** path (internally converted to `^...$`).

```bash
# Match all .rs files
shell-cmd-rs --glob . "echo %1" "*.rs"

# Match all .c and .h files (character class)
shell-cmd-rs --glob . "echo %1" "*.[ch]"

# Match .cpp and .hpp files
shell-cmd-rs --glob . "echo %1" "*.[ch]pp"

# Match files starting with "test" and ending with .py
shell-cmd-rs --glob . "echo %1" "*test*.py"
```

### Combining `--glob` with `--regex-match`

When both `-b` and `-z` are active, the glob is converted to regex and then full-path matching is applied:

```bash
shell-cmd-rs --glob --regex-match . "echo %1" "*cmake*"
```

---

## Exclude Patterns

The `-x` / `--exclude` option skips files and directories whose **filename** (not full path) matches the given pattern. By default, the exclude pattern is a **regex** (substring search):

```bash
# Exclude any file/directory whose name contains "build", "CMakeFiles", or "third_party"
shell-cmd-rs -x "build|CMakeFiles|third_party" . "echo %1" "\.rs$"

# Exclude .git and node_modules
shell-cmd-rs -x "node_modules|\.git" . "wc -l %1" "\.ts$"
```

### `--glob-exclude` / `-i`: Glob Exclude

Add `-i` to treat the `-x` pattern as a **glob** instead of regex. The glob is converted to an anchored regex internally, so it must match the entire filename:

```bash
# Exclude files/dirs whose name matches the glob "build*"
shell-cmd-rs --glob -x "build*" --glob-exclude . "echo %1" "*.rs"

# Exclude object files
shell-cmd-rs --glob -x "*.o" -i . "echo %1" "*.c"
```

### Mixing Regex Exclude with Glob Search

The `-x` pattern and the search pattern are **independent** — you can use `--glob` for the search pattern while keeping `-x` as a regex (the default), or vice versa:

```bash
# Glob search pattern, regex exclude pattern (no -i needed)
shell-cmd-rs --glob -x "build|CMakeFiles|third_party" . "rustfmt %1" "*.rs"

# Regex search pattern, glob exclude pattern (use -i)
shell-cmd-rs -x "build*" -i . "echo %1" "\.rs$"
```

---

## Expression Filter (`--expr`)

The `-f` / `--expr` option lets you compose complex match logic in a single argument, combining `glob()`, `regex()`, and `regex_match()` with boolean operators. When `--expr` is used, the third positional argument (regex) is **not required** — the expression replaces it.

### Grammar

Expressions are built from **functions**, **boolean operators**, and **parentheses**:

| Element | Description |
|---------|-------------|
| `glob("pattern")` | Convert the glob to an anchored regex and apply `regex_search` (same as `--glob`) |
| `regex("pattern")` | Substring regex search (same as default mode) |
| `regex_search("pattern")` | Alias for `regex()` |
| `regex_match("pattern")` | Full-path regex match (same as `--regex-match`) |
| `and` | Both sides must match |
| `or` | Either side must match |
| `not` | Negate the following expression |
| `( … )` | Group sub-expressions to control precedence |

Operator precedence (highest to lowest): `not`, `and`, `or`. Use parentheses to override.

### Examples

Match Rust and TOML files, exclude target directory:

```bash
shell-cmd-rs . "echo %1" --expr '(glob("*.rs") or glob("*.toml")) and not regex("target")'
```

Single function — equivalent to a regex positional argument:

```bash
shell-cmd-rs . "wc -l %1" --expr 'regex("\.py$")'
```

Nested boolean logic — Python or Rust sources, excluding tests and vendor:

```bash
shell-cmd-rs . "echo %1" --expr '(glob("*.py") or glob("*.rs")) and not glob("*test*") and not regex("vendor")'
```

Full-path matching inside an expression:

```bash
shell-cmd-rs . "echo %1" --expr 'regex_match("\\./src/.*\\.rs")'
```

Combine `--expr` with other options (`-x`, `--size`, `--type`):

```bash
shell-cmd-rs -x "node_modules" --size +1K --type f . "wc -l %1" --expr 'glob("*.ts") or glob("*.tsx")'
```

---

## Examples

### Basic Usage

Count lines in all `.rs` files:

```bash
shell-cmd-rs . "wc -l %1" "\.rs$"
```

Dry-run to preview what would be executed:

```bash
shell-cmd-rs -n . "rustfmt %1" "\.rs$"
```

Copy matched files to a destination, using filename-only placeholder:

```bash
shell-cmd-rs . "cp %1 /tmp/backup/%0" "\.txt$"
```

### Depth and Hidden Files

Limit search to current directory (no recursion):

```bash
shell-cmd-rs -d 0 . "cat %1" "\.md$"
```

Include hidden files:

```bash
shell-cmd-rs -a ~ "echo %1" "\.bashrc"
```

### Extra Arguments

Use extra arguments — `%2` is replaced with the value passed after the regex:

```bash
shell-cmd-rs . "cp %1 %2/%0" "\.log$" /tmp/logs
```

Multiple extra arguments:

```bash
shell-cmd-rs . "cp %1 %2/%0 && echo 'copied to %3'" "\.conf$" /backup user@host
```

### Basename and Extension Placeholders

Convert WAV to MP3, using `%b` for the output filename without extension:

```bash
shell-cmd-rs ~/music "ffmpeg -i %1 /tmp/mp3/%b.mp3" "\.wav$"
```

Organize files by extension:

```bash
shell-cmd-rs -n . "mkdir -p /tmp/by-ext/%e && cp %1 /tmp/by-ext/%e/%0" ".*"
```

### List-All Mode

Collect all matches and pass them as a single argument list:

```bash
shell-cmd-rs -l . "cat %0" "\.txt$"
```

Dry-run list-all mode to preview the combined command:

```bash
shell-cmd-rs -l -n . "wc -l %0" "\.rs$"
```

In this mode, `%0` is substituted with a single space-separated string containing every matched path.

### Metadata Filters

Find large files (over 10 MB):

```bash
shell-cmd-rs . "ls -lh %1" ".*" --size +10M
```

Delete files older than 30 days, with dry-run:

```bash
shell-cmd-rs --dry-run /tmp "rm %1" "\.tmp$" --mtime +30
```

Find executable files (permission 755):

```bash
shell-cmd-rs . "echo %1" ".*" --perm 755 --type f
```

List files owned by root:

```bash
shell-cmd-rs /etc "echo %1" "\.conf$" --user root
```

List only directories matching a pattern:

```bash
shell-cmd-rs . "echo %1" "src" --type d
```

Combine filters — large `.log` files modified recently:

```bash
shell-cmd-rs /var/log "wc -l %1" "\.log$" -s +1M -m -7
```

### Glob Mode Examples

Match all Rust source files:

```bash
shell-cmd-rs --glob . "echo %1" "*.rs"
```

Format Rust files, excluding build directories:

```bash
shell-cmd-rs --glob -x "build|target" . "rustfmt %1" "*.rs"
```

Format Rust files, excluding with a glob exclude pattern:

```bash
shell-cmd-rs --glob -x "target*" --glob-exclude . "rustfmt %1" "*.rs"
```

Match files with single-character extensions:

```bash
shell-cmd-rs --glob . "echo %1" "*.?"
```

### Parallel Execution

Run commands in parallel with 4 jobs:

```bash
shell-cmd-rs -j 4 ./images "convert %1 -resize 800x600 /tmp/thumbs/%0" ".*\.jpg$"
```

### Safety Options

Confirm before each destructive command:

```bash
shell-cmd-rs -c /tmp "rm %1" "\.bak$"
```

Stop on first error:

```bash
shell-cmd-rs -e ./src "gcc -c %1 -o /tmp/%b.o" "\.c$"
```

---

## How It Works

The program recursively walks the specified directory using Rust's `std::fs`. For each entry:

1. Hidden files (names starting with `.`) are skipped unless `-a` is set.
2. The **exclude pattern** (`-x`) is tested against the entry's filename. If it matches, the entry (and its subtree, if a directory) is skipped.
3. The **search regex** is tested against the entry's full path.
4. All active **metadata filters** (size, mtime, permissions, owner, group, type) are applied.
5. If everything passes, placeholders in the command template are substituted and the command is executed via `fork`/`execv` through the configured shell.

When using `--glob`, the search pattern and/or exclude pattern (with `-i`) are converted to anchored regex (`^...$`) with proper escaping before matching begins. When using `--regex-match`, the search regex is wrapped in `^(?:...)$` for full-path matching.

When using `-l` / `--list-all`, `shell-cmd-rs` does not run a command per file; it collects all matched paths, joins them with spaces, and runs the command exactly once with `%0` replaced by the full list string.

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
| **Pattern matching** | Rust `regex` crate on the full path; `--regex-match` for full-path anchoring; `--glob` for wildcard patterns; `--expr` for composable expressions | Glob (`-name`) or implementation-varying `-regex` |
| **Exclude patterns** | Built-in `-x` with regex or glob (`-i`) | Requires negation logic or `! -name` |
| **Expression filters** | Built-in `--expr` — combine `glob()`, `regex()`, `regex_match()` with `and`/`or`/`not` | Boolean `-and`/`-or`/`-not` between find predicates |
| **Dry-run** | Built-in `-n` flag | No native support |
| **Verbose mode** | Built-in `-v` flag | No native support |
| **Filter by metadata** | Size (`-s`), time (`-m`), permissions (`-p`), owner (`-u`), group (`-g`), type (`-t`) | Size, time, permissions, ownership, type, boolean logic |
| **Parallel execution** | Built-in `-j N` | Requires `xargs -P` or GNU `parallel` |
| **List-all mode** | Built-in `-l` collects matches into single command | Requires `xargs` or `+` terminator |
| **Confirm mode** | Built-in `-c` flag | Requires `-ok` (not universally supported) |
| **Stop on error** | Built-in `-e` flag | No native support |
| **Summary stats** | Automatic (matched/run/failed counts) | No native support |
| **Portability** | Requires Rust build | POSIX-standard, available everywhere |

Side-by-side example — copy all `.txt` files to a backup directory, preserving filenames:

```bash
# shell-cmd-rs
shell-cmd-rs . "cp %1 /tmp/backup/%0" "\.txt$"

# find equivalent
find . -regex '.*\.txt$' -exec sh -c 'cp "$1" "/tmp/backup/$(basename "$1")"' _ {} \;
```

In short, `shell-cmd-rs` offers a more ergonomic command-templating experience with built-in dry-run, parallel execution, confirm mode, stop-on-error, exclude patterns (regex or glob), composable expression filters (`--expr` with `and`/`or`/`not`), and summary statistics.

## Compatibility

`shell-cmd-rs` is a **drop-in replacement** for `shell-cmd`. All command-line flags, positional arguments, placeholder syntax, output format, and exit codes are identical. You can alias it:

```bash
alias shell-cmd='shell-cmd-rs'
```

## License

GNU GPL v3
