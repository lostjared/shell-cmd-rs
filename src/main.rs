//! # shell-cmd-rs
//!
//! **shell-cmd-rs v1.3** — Recursively find files matching a regex and execute a
//! shell command for each match.
//!
//! This is a drop-in replacement for the C++20 `shell-cmd` utility, rewritten
//! entirely in Rust. It walks a directory tree, applies metadata filters (size,
//! modification time, permissions, ownership, type), substitutes placeholders in
//! a command template, and executes the resulting command for every matched entry.
//!
//! ## Features
//!
//! - Regex-based file matching (via the `regex` crate)
//! - Two regex modes:
//!   - **regex-search** (default): matches if the pattern appears anywhere in the
//!     full path (substring match via `Regex::is_match`)
//!   - **regex-match** (`-z`/`--regex-match`): the pattern must match the **entire**
//!     path (anchored with `^(?:...)$`)
//! - Glob mode (`-b`/`--glob`): use familiar wildcard patterns (`*`, `?`)
//!   instead of regex — special characters are auto-escaped
//! - Expression filter (`-f`/`--expr`): compose `glob()`, `regex()`,
//!   `regex_search()`, and `regex_match()` predicates with boolean operators
//!   `and`, `or`, `not`, and parentheses
//! - Placeholder substitution: `%0` (filename), `%1` (full path), `%b` (stem),
//!   `%e` (extension), `%2+` (extra args); in `--list-all` mode `%0` expands to
//!   all matched paths joined by spaces
//! - Metadata filters: size, modification time, permissions, owner, group, type
//! - Exclude patterns (regex by default, or glob via `-i`/`--glob-exclude`),
//!   dry-run, verbose, confirm mode, stop-on-error
//! - Parallel execution via `fork`/`execv` with proper signal handling
//! - List-all mode (`-l`/`--list-all`): collect all matches and run the command
//!   once with `%0` expanded to the full list of matched paths
//! - Summary statistics (matched/run/failed)
//!
//! ## Architecture
//!
//! The program flow is:
//! 1. Parse CLI arguments via `clap` derive macros into [`Cli`]
//! 2. Convert [`Cli`] into [`Options`] (runtime config)
//! 3. Compile regex patterns
//! 4. In list-all mode (`-l`), call [`fill_list()`] to collect all matches into a
//!    vector, then invoke [`proc_cmd()`] once with `%0` expanded to all paths
//! 5. Otherwise, call [`add_directory()`] to recursively walk the filesystem
//!    and call [`proc_cmd()`] per match to substitute placeholders and execute
//! 6. In parallel mode, manage child PIDs via [`CHILD_PIDS`] and drain with [`wait_all()`]
//! 7. Print summary to stderr
//!
//! ## Signal Handling
//!
//! Command execution uses [`system_cmd()`], which mirrors the POSIX `system()`
//! behavior with proper `SIGCHLD` blocking and `SIGINT`/`SIGQUIT` ignoring in
//! the parent process. This prevents Ctrl+C from killing the batch runner while
//! allowing it to reach child processes.

use clap::Parser;
use regex::Regex;
use std::ffi::CString;
use std::fs;
use std::io::{self, BufRead, Write};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::SystemTime;

/// Check whether to use color output on the given file descriptor.
/// Respects the NO_COLOR environment variable convention (<https://no-color.org/>).
fn use_color(fd: i32) -> bool {
    if std::env::var_os("NO_COLOR").is_some() {
        return false;
    }
    unsafe { libc::isatty(fd) != 0 }
}

/// Print a colored error message to stderr.
/// Prefixes the message with "Error: " (bold red when color is enabled).
macro_rules! error {
    ($($arg:tt)*) => {{
        let msg = format!($($arg)*);
        if use_color(2) {
            eprintln!("\x1b[1;31mError:\x1b[0m {}", msg);
        } else {
            eprintln!("Error: {}", msg);
        }
    }};
}

/// Global flag set to `true` when `--stop-on-error` is active and a command has
/// failed. Checked at the top of each iteration in [`add_directory()`] and
/// [`proc_cmd()`] to halt processing early. Uses `SeqCst` ordering since it is
/// only written once and read from a single thread (parallel children don't read it).
static STOP_REQUESTED: AtomicBool = AtomicBool::new(false);

/// Global flag set to `true` when SIGINT (Ctrl+C) is received.
/// Checked alongside `STOP_REQUESTED` to halt processing and exit cleanly.
static INTERRUPTED: AtomicBool = AtomicBool::new(false);

/// Global pool of outstanding child process PIDs, used only in parallel mode
/// (`-j N` where N > 1). Protected by a `Mutex` since we access it from the
/// main thread only (no actual concurrent access, but the Mutex satisfies Rust's
/// `Send`/`Sync` requirements for statics). Initialized lazily on first access.
static CHILD_PIDS: std::sync::LazyLock<Mutex<Vec<nix::unistd::Pid>>> =
    std::sync::LazyLock::new(|| Mutex::new(Vec::new()));

/// Comparison operator for size and time filters.
///
/// Used by [`SizeFilter`] and [`TimeFilter`] to determine
/// the comparison direction when testing metadata values.
///
/// - `Eq` — exact match (no prefix in the CLI string)
/// - `Lt` — less than (CLI prefix `-`)
/// - `Gt` — greater than (CLI prefix `+`)
#[derive(Clone, Copy)]
enum CmpOp {
    /// Exact equality (e.g., `4096` means exactly 4096 bytes, `3` means exactly 3 days old).
    Eq,
    /// Less than (e.g., `-1K` means smaller than 1 KB, `-1` means newer than 1 day).
    Lt,
    /// Greater than (e.g., `+10M` means larger than 10 MB, `+7` means older than 7 days).
    Gt,
}

/// Parsed size filter with comparison operator and byte threshold.
///
/// Created by [`parse_size_filter()`] from CLI strings like `+10M`, `-1K`, or `4096`.
/// The `active` field indicates whether this filter was specified on the command line.
///
/// # Size suffixes
///
/// - `K` / `k` — multiply by 1024
/// - `M` / `m` — multiply by 1024²
/// - `G` / `g` — multiply by 1024³
/// - No suffix — raw bytes
#[derive(Clone)]
struct SizeFilter {
    /// Whether this filter is enabled (a `--size` value was provided).
    active: bool,
    /// Comparison direction: exact, less-than, or greater-than.
    op: CmpOp,
    /// Size threshold in bytes (after applying any K/M/G multiplier).
    bytes: u64,
}

/// Parsed modification-time filter with comparison operator and day count.
///
/// Created by [`parse_time_filter()`] from CLI strings like `+7`, `-1`, or `3`.
/// The `active` field indicates whether this filter was specified on the command line.
///
/// # Semantics
///
/// - `+N` — file is older than N days (age > N)
/// - `-N` — file is newer than N days (age < N)
/// - `N`  — file is exactly N days old (age == N)
///
/// Age is computed as `(now - mtime) / 86400` (integer division, in hours/24).
#[derive(Clone)]
struct TimeFilter {
    /// Whether this filter is enabled (a `--mtime` value was provided).
    active: bool,
    /// Comparison direction: exact, less-than, or greater-than.
    op: CmpOp,
    /// Age threshold in days.
    days: i64,
}

/// Execution statistics printed in the summary line.
///
/// Tracks how many files matched, how many commands were executed (or would have
/// been in dry-run mode), and how many commands returned a non-zero exit code.
/// The summary is printed to stderr at the end of execution when verbose, dry-run,
/// or any command has failed.
struct Stats {
    /// Number of directory entries that matched the regex and all metadata filters.
    files_matched: i32,
    /// Number of commands executed (or printed in dry-run mode).
    commands_run: i32,
    /// Number of commands that returned a non-zero exit status.
    commands_failed: i32,
}

/// Command-line interface definition using `clap` derive macros.
///
/// This struct defines every flag and option that `shell-cmd-rs` accepts.
/// Positional arguments (path, command, regex, extras) are collected into
/// the `args` field via `trailing_var_arg`.
///
/// # Compatibility
///
/// The short flags, long flags, and their semantics are identical to the
/// original C++ `shell-cmd` to ensure drop-in compatibility.
#[derive(Parser)]
#[command(
    name = "shell-cmd-rs",
    version = "1.3.0",
    about = "Recursively find files matching regex and run command for each.",
    after_help = "\
placeholders:
  %0          filename only (no path)
  %1          full path to matched file
  %2+         extra arguments from command line
  %b          basename without extension
  %e          file extension (including dot)

regex modes:
  (default)   regex-search — pattern matches anywhere in the path
  -z          regex-match  — pattern must match the entire path

glob mode:
  -b          treat pattern as a glob (*, ?) instead of regex
              special regex characters are auto-escaped

expr mode:
  -f EXPR     compose glob(), regex(), regex_match() with and/or/not
              e.g. --expr '(glob(\"*.cpp\") or glob(\"*.hpp\")) and not regex(\"build\")'"
)]
struct Cli {
    /// Dry-run, print commands without executing
    #[arg(short = 'n', long = "dry-run")]
    dry_run: bool,

    /// Verbose, print each command before running
    #[arg(short = 'v', long = "verbose")]
    verbose: bool,

    /// Include hidden files/directories
    #[arg(short = 'a', long = "all")]
    all: bool,

    /// Max recursion depth (0 = current dir only)
    #[arg(short = 'd', long = "depth")]
    depth: Option<i32>,

    /// Filter by size: +10M (>10MB), -1K (<1KB), 4096 (exactly 4096 bytes). Suffixes: K, M, G
    #[arg(short = 's', long = "size")]
    size: Option<String>,

    /// Filter by modification time: +7 (older than 7 days), -1 (modified within last day), 3 (exactly 3 days)
    #[arg(short = 'm', long = "mtime")]
    mtime: Option<String>,

    /// Filter by permissions (octal), e.g. 755
    #[arg(short = 'p', long = "perm")]
    perm: Option<String>,

    /// Filter by owner username
    #[arg(short = 'u', long = "user")]
    user: Option<String>,

    /// Filter by group name
    #[arg(short = 'g', long = "group")]
    group: Option<String>,

    /// Filter by type: f (file), d (directory), l (symlink)
    #[arg(short = 't', long = "type")]
    type_filter: Option<char>,

    /// Exclude files/directories matching REGEX
    #[arg(short = 'x', long = "exclude")]
    exclude: Option<String>,

    /// Stop on first command failure
    #[arg(short = 'e', long = "stop-on-error")]
    stop_on_error: bool,

    /// Prompt for confirmation before each command
    #[arg(short = 'c', long = "confirm")]
    confirm: bool,

    /// Collect all matches and run command once with %0 = all matched paths
    #[arg(short = 'l', long = "list-all")]
    list_all: bool,

    /// Run N commands in parallel (default: 1)
    #[arg(short = 'j', long = "jobs", default_value = "1")]
    jobs: i32,

    /// Shell to use for execution (default: /bin/bash)
    #[arg(short = 'w', long = "shell", default_value = "/bin/bash")]
    shell: String,

    /// Positional args: path "command" regex [extra_args..]
    args: Vec<String>,

    /// Treat pattern as a glob (*, ?) instead of regex
    #[arg(short = 'b', long = "glob")]
    glob: bool,

    /// Use regex-match (entire path must match) instead of regex-search (substring match)
    #[arg(short = 'z', long = "regex-match")]
    regex_match: bool,

    /// Treat exclude pattern as a glob (*, ?) instead of regex
    #[arg(short = 'i', long = "glob-exclude")]
    glob_exclude: bool,

    /// Expression filter: compose glob(), regex(), regex_match() with and/or/not
    #[arg(short = 'f', long = "expr")]
    expr: Option<String>,
}

/// Aggregated runtime options parsed from CLI arguments.
///
/// This is a flattened, validated view of [`Cli`] that the rest of the program
/// operates on. Filters that weren't specified on the command line have their
/// `active` flag set to false or their string fields left empty.
struct Options {
    /// Print commands without executing them.
    dry_run: bool,
    /// Print each command to stdout before executing it.
    verbose: bool,
    /// Include hidden (dot-prefixed) files and directories.
    hidden: bool,
    /// Maximum recursion depth (-1 = unlimited, 0 = current dir only).
    max_depth: i32,
    /// Optional size filter (e.g., `+10M`).
    size_filter: SizeFilter,
    /// Optional modification-time filter (e.g., `+7`).
    mtime_filter: TimeFilter,
    /// Octal permission string for filtering (e.g., `"755"`). Empty = disabled.
    perm_filter: String,
    /// Owner username to filter by. Empty = disabled.
    user_filter: String,
    /// Group name to filter by. Empty = disabled.
    group_filter: String,
    /// Type filter character: `'f'` (file), `'d'` (dir), `'l'` (symlink), `'\0'` = disabled.
    type_filter: char,
    /// Regex pattern string for excluding entries. Empty = disabled.
    exclude_pattern: String,
    /// Halt processing on the first command that returns non-zero.
    stop_on_error: bool,
    /// Prompt the user for y/N confirmation before each command.
    confirm: bool,
    /// Number of parallel child processes (1 = sequential, >1 = fork pool).
    jobs: i32,
    /// Shell path to use for command execution (default: /bin/bash).
    shell: String,
    shell_name: String,
    /// If true (via `-l`/`--list-all`), collect all matched file paths and run
    /// one command with `%0` expanded to the combined space-delimited list.
    collect_all: bool,
    /// If true (via `-b`/`--glob`), treat patterns as globs instead of regex.
    glob: bool,
    /// If true (via `-z`/`--regex-match`), the regex must match the **entire**
    /// path (anchored with `^(?:...)$`) rather than just a substring.
    regex_match: bool,
    /// If true (via `-i`/`--glob-exclude`), treat the exclude pattern as a glob
    /// instead of regex.
    glob_exclude: bool,
    /// Expression filter string from `--expr`.
    expr_str: String,
}

/// Convert a glob pattern to an equivalent regex string.
///
/// Escapes regex-special characters and translates glob wildcards:
/// - `*` becomes `.*`
/// - `?` becomes `.`
/// - `[...]` character classes are passed through (with `!` or `^` mapped to `^`)
/// - All other regex metacharacters are escaped with a backslash.
/// - The result is anchored with `^...$`.
///
/// # Examples
///
/// - `"*.cpp"` → `"^.*\.cpp$"`
/// - `"test?"` → `"^test.$"`
/// - `"*cmake"` → `"^.*cmake$"`
/// - `"[!a-z]*"` → `"^[^a-z].*$"`
///
/// Used by `--glob` to convert the search pattern and by `--glob-exclude` (`-i`)
/// to convert the exclude pattern.
fn glob_to_regex(glob: &str) -> String {
    let mut result = String::from('^');
    let chars: Vec<char> = glob.chars().collect();
    let mut i = 0;
    let mut in_class = false;

    while i < chars.len() {
        let c = chars[i];

        if in_class {
            if c == ']' {
                in_class = false;
                result.push(']');
            } else if c == '\\' {
                result.push_str("\\\\");
            } else {
                result.push(c);
            }
            i += 1;
            continue;
        }

        match c {
            '*' => result.push_str(".*"),
            '?' => result.push('.'),
            '[' => {
                in_class = true;
                result.push('[');
                if i + 1 < chars.len() && (chars[i + 1] == '!' || chars[i + 1] == '^') {
                    result.push('^');
                    i += 1;
                }
            }
            '.' | '\\' | '+' | '^' | '$' | '|' | '(' | ')' | '{' | '}' => {
                result.push('\\');
                result.push(c);
            }
            _ => result.push(c),
        }
        i += 1;
    }

    if in_class {
        result.push('\\');
    }

    result.push('$');
    result
}

// --- Expression filter (--expr) ------------------------------------------------

/// Node types for the expression filter AST.
enum ExprType {
    Glob,
    RegexSearch,
    RegexMatch,
    And,
    Or,
    Not,
}

/// AST node for expression-based file matching.
struct ExprNode {
    node_type: ExprType,
    /// Pre-compiled regex (leaf nodes only).
    compiled: Option<Regex>,
    /// Left child (AND/OR) or sole child (NOT).
    left: Option<Box<ExprNode>>,
    /// Right child (AND/OR only).
    right: Option<Box<ExprNode>>,
}

impl ExprNode {
    /// Evaluate this expression node against a file path.
    fn evaluate(&self, path: &str) -> bool {
        match self.node_type {
            ExprType::Glob | ExprType::RegexSearch => {
                self.compiled.as_ref().map_or(false, |re| re.is_match(path))
            }
            ExprType::RegexMatch => self
                .compiled
                .as_ref()
                .map_or(false, |re| re.is_match(path)),
            ExprType::And => {
                self.left.as_ref().map_or(false, |l| l.evaluate(path))
                    && self.right.as_ref().map_or(false, |r| r.evaluate(path))
            }
            ExprType::Or => {
                self.left.as_ref().map_or(false, |l| l.evaluate(path))
                    || self.right.as_ref().map_or(false, |r| r.evaluate(path))
            }
            ExprType::Not => !self.left.as_ref().map_or(false, |l| l.evaluate(path)),
        }
    }
}

/// Token produced by the expression tokenizer.
#[derive(Debug, Clone, PartialEq)]
enum ExprTokenType {
    Ident,
    StringLit,
    LParen,
    RParen,
    End,
}

#[derive(Debug, Clone)]
struct ExprToken {
    token_type: ExprTokenType,
    value: String,
}

/// Tokenizer for expression filter strings.
struct ExprTokenizer {
    chars: Vec<char>,
    pos: usize,
}

impl ExprTokenizer {
    fn new(src: &str) -> Self {
        Self {
            chars: src.chars().collect(),
            pos: 0,
        }
    }

    fn skip_ws(&mut self) {
        while self.pos < self.chars.len() && self.chars[self.pos].is_whitespace() {
            self.pos += 1;
        }
    }

    fn next_token(&mut self) -> ExprToken {
        self.skip_ws();
        if self.pos >= self.chars.len() {
            return ExprToken {
                token_type: ExprTokenType::End,
                value: String::new(),
            };
        }
        let c = self.chars[self.pos];
        if c == '(' {
            self.pos += 1;
            return ExprToken {
                token_type: ExprTokenType::LParen,
                value: "(".to_string(),
            };
        }
        if c == ')' {
            self.pos += 1;
            return ExprToken {
                token_type: ExprTokenType::RParen,
                value: ")".to_string(),
            };
        }
        if c == '"' || c == '\'' {
            let q = c;
            self.pos += 1;
            let mut val = String::new();
            while self.pos < self.chars.len() && self.chars[self.pos] != q {
                if self.chars[self.pos] == '\\' && self.pos + 1 < self.chars.len() {
                    self.pos += 1;
                    val.push(self.chars[self.pos]);
                } else {
                    val.push(self.chars[self.pos]);
                }
                self.pos += 1;
            }
            if self.pos < self.chars.len() {
                self.pos += 1;
            }
            return ExprToken {
                token_type: ExprTokenType::StringLit,
                value: val,
            };
        }
        if c.is_alphabetic() || c == '_' {
            let mut val = String::new();
            while self.pos < self.chars.len()
                && (self.chars[self.pos].is_alphanumeric() || self.chars[self.pos] == '_')
            {
                val.push(self.chars[self.pos]);
                self.pos += 1;
            }
            return ExprToken {
                token_type: ExprTokenType::Ident,
                value: val,
            };
        }
        error!(
            "unexpected character '{}' in expression at position {}",
            c, self.pos
        );
        process::exit(1);
    }
}

/// Recursive-descent parser for expression filter strings.
///
/// Grammar:
///   expr     := or_expr
///   or_expr  := and_expr ("or" and_expr)*
///   and_expr := not_expr ("and" not_expr)*
///   not_expr := "not" not_expr | primary
///   primary  := function "(" STRING ")" | "(" expr ")"
///   function := "glob" | "regex" | "regex_search" | "regex_match"
struct ExprParser {
    tok: ExprTokenizer,
    cur: ExprToken,
}

impl ExprParser {
    fn new(src: &str) -> Self {
        let mut tok = ExprTokenizer::new(src);
        let cur = tok.next_token();
        Self { tok, cur }
    }

    fn advance(&mut self) {
        self.cur = self.tok.next_token();
    }

    fn expect(&mut self, t: ExprTokenType, desc: &str) {
        if self.cur.token_type != t {
            let got = if self.cur.value.is_empty() {
                "end"
            } else {
                &self.cur.value
            };
            error!("expected {} in expression, got '{}'", desc, got);
            process::exit(1);
        }
        self.advance();
    }

    fn parse_primary(&mut self) -> Box<ExprNode> {
        if self.cur.token_type == ExprTokenType::LParen {
            self.advance();
            let node = self.parse_or();
            self.expect(ExprTokenType::RParen, "')'");
            return node;
        }
        if self.cur.token_type != ExprTokenType::Ident {
            let got = if self.cur.value.is_empty() {
                "end"
            } else {
                &self.cur.value
            };
            error!("unexpected token '{}' in expression", got);
            process::exit(1);
        }
        let name = self.cur.value.clone();
        let ft = match name.as_str() {
            "glob" => ExprType::Glob,
            "regex" | "regex_search" => ExprType::RegexSearch,
            "regex_match" => ExprType::RegexMatch,
            _ => {
                error!("unknown function '{}' in expression", name);
                process::exit(1);
            }
        };
        self.advance();
        self.expect(ExprTokenType::LParen, "'(' after function name");
        if self.cur.token_type != ExprTokenType::StringLit {
            error!("expected quoted string as function argument");
            process::exit(1);
        }
        let pattern = self.cur.value.clone();
        self.advance();
        self.expect(ExprTokenType::RParen, "')'");

        let regex_pattern = match ft {
            ExprType::Glob => glob_to_regex(&pattern),
            ExprType::RegexMatch => format!("^(?:{})$", pattern),
            _ => pattern.clone(),
        };
        let compiled = Regex::new(&regex_pattern).unwrap_or_else(|e| {
            error!("invalid regex '{}' in expression: {}", pattern, e);
            process::exit(1);
        });

        Box::new(ExprNode {
            node_type: ft,
            compiled: Some(compiled),
            left: None,
            right: None,
        })
    }

    fn parse_not(&mut self) -> Box<ExprNode> {
        if self.cur.token_type == ExprTokenType::Ident && self.cur.value == "not" {
            self.advance();
            let child = self.parse_not();
            return Box::new(ExprNode {
                node_type: ExprType::Not,
                compiled: None,
                left: Some(child),
                right: None,
            });
        }
        self.parse_primary()
    }

    fn parse_and(&mut self) -> Box<ExprNode> {
        let mut left = self.parse_not();
        while self.cur.token_type == ExprTokenType::Ident && self.cur.value == "and" {
            self.advance();
            let right = self.parse_not();
            left = Box::new(ExprNode {
                node_type: ExprType::And,
                compiled: None,
                left: Some(left),
                right: Some(right),
            });
        }
        left
    }

    fn parse_or(&mut self) -> Box<ExprNode> {
        let mut left = self.parse_and();
        while self.cur.token_type == ExprTokenType::Ident && self.cur.value == "or" {
            self.advance();
            let right = self.parse_and();
            left = Box::new(ExprNode {
                node_type: ExprType::Or,
                compiled: None,
                left: Some(left),
                right: Some(right),
            });
        }
        left
    }

    fn parse(mut self) -> Box<ExprNode> {
        let root = self.parse_or();
        if self.cur.token_type != ExprTokenType::End {
            error!("unexpected content after expression");
            process::exit(1);
        }
        root
    }
}

/// Check whether a path matches the active search pattern or expression.
fn entry_matches_path(
    fullpath: &str,
    regex: &Regex,
    expr_root: Option<&ExprNode>,
) -> bool {
    if let Some(root) = expr_root {
        return root.evaluate(fullpath);
    }
    regex.is_match(fullpath)
}

/// Parse a size filter string into a [`SizeFilter`].
///
/// # Format
///
/// The input string has the form `[+|-]<number>[K|M|G]`:
/// - Prefix `+` → greater-than comparison
/// - Prefix `-` → less-than comparison
/// - No prefix  → exact equality
/// - Suffix `K`/`k` → multiply by 1024
/// - Suffix `M`/`m` → multiply by 1024²
/// - Suffix `G`/`g` → multiply by 1024³
///
/// # Examples
///
/// - `"+10M"` → greater than 10 MiB
/// - `"-1K"` → less than 1 KiB
/// - `"4096"` → exactly 4096 bytes
///
/// # Panics
///
/// Prints an error and calls `process::exit(1)` if the numeric part cannot be parsed.
fn parse_size_filter(s: &str) -> SizeFilter {
    // Determine comparison operator from the first character:
    // '+' means "greater than", '-' means "less than", anything else means "exact".
    let mut val = s;
    let op = if val.starts_with('+') {
        val = &val[1..];
        CmpOp::Gt
    } else if val.starts_with('-') {
        val = &val[1..];
        CmpOp::Lt
    } else {
        CmpOp::Eq
    };

    // Check for a size suffix (K/M/G) and compute the byte multiplier.
    // The suffix is case-insensitive; if present, it's stripped from the numeric part.
    let (num_str, multiplier) = if val.ends_with('K') || val.ends_with('k') {
        (&val[..val.len() - 1], 1024u64)
    } else if val.ends_with('M') || val.ends_with('m') {
        (&val[..val.len() - 1], 1024u64 * 1024)
    } else if val.ends_with('G') || val.ends_with('g') {
        (&val[..val.len() - 1], 1024u64 * 1024 * 1024)
    } else {
        (val, 1u64)
    };

    // Parse the numeric portion and multiply by the suffix multiplier.
    // Exit with an error if the number cannot be parsed.
    let bytes = num_str.parse::<u64>().unwrap_or_else(|_| {
        error!("invalid size value '{s}'");
        process::exit(1);
    }) * multiplier;

    SizeFilter {
        active: true,
        op,
        bytes,
    }
}

/// Parse a time filter string into a [`TimeFilter`].
///
/// # Format
///
/// The input string has the form `[+|-]<number>`:
/// - Prefix `+` → older than N days (age > N)
/// - Prefix `-` → newer than N days (age < N)
/// - No prefix  → exactly N days old (age == N)
///
/// # Examples
///
/// - `"+7"` → older than 7 days
/// - `"-1"` → modified within the last day
/// - `"3"` → exactly 3 days old
///
/// # Panics
///
/// Prints an error and calls `process::exit(1)` if the numeric part cannot be parsed.
fn parse_time_filter(s: &str) -> TimeFilter {
    // Determine comparison operator from the first character,
    // same logic as parse_size_filter.
    let mut val = s;
    let op = if val.starts_with('+') {
        val = &val[1..];
        CmpOp::Gt
    } else if val.starts_with('-') {
        val = &val[1..];
        CmpOp::Lt
    } else {
        CmpOp::Eq
    };

    let days = val.parse::<i64>().unwrap_or_else(|_| {
        error!("invalid time value '{s}'");
        process::exit(1);
    });

    TimeFilter {
        active: true,
        op,
        days,
    }
}

/// Test a directory entry's metadata against all active filters.
///
/// This function checks the entry's type, size, modification time, permissions,
/// owner, and group against the corresponding filter in [`Options`]. If any
/// active filter fails, returns `false`; otherwise returns `true`.
///
/// # Arguments
///
/// - `_path` — the filesystem path (currently unused but available for future use)
/// - `metadata` — the `std::fs::Metadata` for the entry (may be symlink or resolved)
/// - `opts` — the runtime options containing all filter configurations
///
/// # Filter evaluation order
///
/// 1. Type filter (`-t`)
/// 2. Size filter (`-s`) — only applies to regular files
/// 3. Modification time filter (`-m`)
/// 4. Permission filter (`-p`) — compares `mode & 0o7777` against octal target
/// 5. User filter (`-u`) — looks up UID → username via `getpwuid()`
/// 6. Group filter (`-g`) — looks up GID → group name via `getgrgid()`
fn matches_filters(_path: &Path, metadata: &fs::Metadata, opts: &Options) -> bool {
    let ft = metadata.file_type();

    // --- Type filter ---
    // Check if the entry matches the requested type (file/dir/symlink).
    // If no type filter is set (type_filter == '\0'), this check is skipped.
    if opts.type_filter != '\0' {
        match opts.type_filter {
            'f' => {
                if !ft.is_file() {
                    return false;
                }
            }
            'd' => {
                if !ft.is_dir() {
                    return false;
                }
            }
            'l' => {
                if !ft.is_symlink() {
                    return false;
                }
            }
            _ => {}
        }
    }

    // Size filter (only meaningful for regular files)
    if opts.size_filter.active {
        if !ft.is_file() {
            return false;
        }
        let sz = metadata.len();
        match opts.size_filter.op {
            CmpOp::Gt => {
                if sz <= opts.size_filter.bytes {
                    return false;
                }
            }
            CmpOp::Lt => {
                if sz >= opts.size_filter.bytes {
                    return false;
                }
            }
            CmpOp::Eq => {
                if sz != opts.size_filter.bytes {
                    return false;
                }
            }
        }
    }

    // Modification time filter
    if opts.mtime_filter.active {
        if let Ok(mtime) = metadata.modified() {
            if let Ok(elapsed) = SystemTime::now().duration_since(mtime) {
                let age_days = (elapsed.as_secs() / 86400) as i64;
                match opts.mtime_filter.op {
                    CmpOp::Gt => {
                        if age_days <= opts.mtime_filter.days {
                            return false;
                        }
                    }
                    CmpOp::Lt => {
                        if age_days >= opts.mtime_filter.days {
                            return false;
                        }
                    }
                    CmpOp::Eq => {
                        if age_days != opts.mtime_filter.days {
                            return false;
                        }
                    }
                }
            } else {
                return false;
            }
        } else {
            return false;
        }
    }

    // Permission filter (octal comparison)
    if !opts.perm_filter.is_empty() {
        let mode = metadata.mode() & 0o7777;
        let target = u32::from_str_radix(&opts.perm_filter, 8).unwrap_or_else(|_| {
            error!("invalid permission filter '{}'", opts.perm_filter);
            process::exit(1);
        });
        if mode != target {
            return false;
        }
    }

    // User filter
    if !opts.user_filter.is_empty() {
        let uid = metadata.uid();
        let name = uid_to_name(uid);
        if name.as_deref() != Some(opts.user_filter.as_str()) {
            return false;
        }
    }

    // Group filter
    if !opts.group_filter.is_empty() {
        let gid = metadata.gid();
        let name = gid_to_name(gid);
        if name.as_deref() != Some(opts.group_filter.as_str()) {
            return false;
        }
    }

    true
}

fn uid_to_name(uid: u32) -> Option<String> {
    // Safety: getpwuid is a standard POSIX call
    unsafe {
        let pw = libc::getpwuid(uid);
        if pw.is_null() {
            return None;
        }
        let cstr = std::ffi::CStr::from_ptr((*pw).pw_name);
        Some(cstr.to_string_lossy().into_owned())
    }
}

fn gid_to_name(gid: u32) -> Option<String> {
    // Safety: getgrgid is a standard POSIX call
    unsafe {
        let gr = libc::getgrgid(gid);
        if gr.is_null() {
            return None;
        }
        let cstr = std::ffi::CStr::from_ptr((*gr).gr_name);
        Some(cstr.to_string_lossy().into_owned())
    }
}

fn replace_all(orig: &str, from: &str, to: &str) -> String {
    orig.replace(from, to)
}

/// Execute a shell command via fork/exec with proper signal handling (mirrors the C++ System()).
fn system_cmd(command: &str, opts: &Options) -> i32 {
    if command.is_empty() {
        return if system_cmd(":", opts) == 0 { 1 } else { 0 };
    }

    let c_command = CString::new(command).unwrap_or_else(|_| {
        error!("command contains null byte");
        process::exit(1);
    });
    let c_sh = CString::new(opts.shell.as_str()).unwrap();
    let c_sh_arg = CString::new(opts.shell_name.as_str()).unwrap();
    let c_c = CString::new("-c").unwrap();

    // Block SIGCHLD
    let mut block_set = nix::sys::signal::SigSet::empty();
    block_set.add(nix::sys::signal::Signal::SIGCHLD);
    let mut old_mask = nix::sys::signal::SigSet::empty();
    let has_old_mask = nix::sys::signal::sigprocmask(
        nix::sys::signal::SigmaskHow::SIG_BLOCK,
        Some(&block_set),
        Some(&mut old_mask),
    )
    .is_ok();

    // Use our sigint_handler instead of SIG_IGN so Ctrl+C is recorded
    let sa_int_handler = nix::sys::signal::SigAction::new(
        nix::sys::signal::SigHandler::Handler(sigint_handler),
        nix::sys::signal::SaFlags::empty(),
        nix::sys::signal::SigSet::empty(),
    );
    let sa_ignore = nix::sys::signal::SigAction::new(
        nix::sys::signal::SigHandler::SigIgn,
        nix::sys::signal::SaFlags::empty(),
        nix::sys::signal::SigSet::empty(),
    );
    let old_sigint =
        unsafe { nix::sys::signal::sigaction(nix::sys::signal::Signal::SIGINT, &sa_int_handler) }
            .ok();
    let old_sigquit =
        unsafe { nix::sys::signal::sigaction(nix::sys::signal::Signal::SIGQUIT, &sa_ignore) }.ok();

    let status = match unsafe { nix::unistd::fork() } {
        Ok(nix::unistd::ForkResult::Child) => {
            // Restore default signal handlers in child
            let sa_default = nix::sys::signal::SigAction::new(
                nix::sys::signal::SigHandler::SigDfl,
                nix::sys::signal::SaFlags::empty(),
                nix::sys::signal::SigSet::empty(),
            );
            if let Some(ref old) = old_sigint {
                if old.handler() != nix::sys::signal::SigHandler::SigIgn {
                    unsafe {
                        let _ = nix::sys::signal::sigaction(
                            nix::sys::signal::Signal::SIGINT,
                            &sa_default,
                        );
                    }
                }
            }
            if let Some(ref old) = old_sigquit {
                if old.handler() != nix::sys::signal::SigHandler::SigIgn {
                    unsafe {
                        let _ = nix::sys::signal::sigaction(
                            nix::sys::signal::Signal::SIGQUIT,
                            &sa_default,
                        );
                    }
                }
            }
            // Restore signal mask in child
            if has_old_mask {
                let _ = nix::sys::signal::sigprocmask(
                    nix::sys::signal::SigmaskHow::SIG_SETMASK,
                    Some(&old_mask),
                    None,
                );
            }

            nix::unistd::execv(
                &c_sh,
                &[c_sh_arg.as_c_str(), c_c.as_c_str(), c_command.as_c_str()],
            )
            .ok();
            unsafe { libc::_exit(127) };
        }
        Ok(nix::unistd::ForkResult::Parent { child }) => loop {
            match nix::sys::wait::waitpid(child, None) {
                Ok(ws) => match ws {
                    nix::sys::wait::WaitStatus::Exited(_, code) => {
                        // Shell catches SIGINT and exits with 130 (128+2)
                        if code == 130 {
                            INTERRUPTED.store(true, Ordering::SeqCst);
                        }
                        break code;
                    }
                    nix::sys::wait::WaitStatus::Signaled(_, sig, _) => {
                        // If the child was killed directly by SIGINT
                        if sig == nix::sys::signal::Signal::SIGINT {
                            INTERRUPTED.store(true, Ordering::SeqCst);
                        }
                        break -1;
                    }
                    _ => break -1,
                },
                Err(nix::errno::Errno::EINTR) => continue,
                Err(_) => break -1,
            }
        },
        Err(_) => -1,
    };

    // Restore signal mask and handlers
    if has_old_mask {
        let _ = nix::sys::signal::sigprocmask(
            nix::sys::signal::SigmaskHow::SIG_SETMASK,
            Some(&old_mask),
            None,
        );
    }
    if let Some(ref old) = old_sigint {
        unsafe {
            let _ = nix::sys::signal::sigaction(nix::sys::signal::Signal::SIGINT, old);
        }
    }
    if let Some(ref old) = old_sigquit {
        unsafe {
            let _ = nix::sys::signal::sigaction(nix::sys::signal::Signal::SIGQUIT, old);
        }
    }

    status
}

/// Substitute placeholders in a command template and execute the result.
///
/// # Modes
///
/// - **Default mode** (one invocation per match): `%0` = basename, `%1` = full
///   path, `%2+` = extra args, `%b` = stem, `%e` = extension.
/// - **`--list-all` mode** (`-l`): all matching file paths are collected first
///   by [`fill_list()`], joined into a single space-delimited string, and passed
///   as `file_string`. In this mode `%0` is replaced with the entire list of
///   matched paths rather than an individual filename.
///
/// Supports confirm mode, dry-run, parallel forking, and stop-on-error.
///
/// # Arguments
///
/// - `cmd` — the command template string containing `%` placeholders
/// - `text` — slice of strings: `text[0]` is the matched file path (unused in
///   list-all mode), `text[1+]` are extra CLI arguments
/// - `file_string` — when `--list-all` is active, the space-joined list of all
///   matched paths; `None` in default per-file mode
/// - `opts` — runtime options
/// - `stats` — mutable execution statistics
///
/// # Returns
///
/// `true` to continue processing, `false` to stop (stop-on-error triggered).
fn proc_cmd(
    cmd: &str,
    text: &[String],
    file_string: Option<&str>,
    opts: &Options,
    stats: &mut Stats,
) -> bool {
    let mut r = cmd.to_string();
    if file_string.is_none() && !text.is_empty() {
        let fpath = Path::new(&text[0]);
        let fname = fpath
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_default();
        let stem = fpath
            .file_stem()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_default();
        let ext = fpath
            .extension()
            .map(|s| format!(".{}", s.to_string_lossy()))
            .unwrap_or_default();
        r = replace_all(&r, "%0", &fname);
        r = replace_all(&r, "%b", &stem);
        r = replace_all(&r, "%e", &ext);
    }
    if let Some(fs) = file_string {
        // --list-all mode: %0 expands to the full list of matched paths
        r = replace_all(&r, "%0", fs);
        for (i, val) in text.iter().enumerate() {
            let placeholder = format!("%{}", i + 1);
            if val.contains(' ') {
                r = replace_all(&r, &placeholder, &format!("\"{}\"", val));
            } else {
                r = replace_all(&r, &placeholder, val);
            }
        }
    } else {
        for (i, val) in text.iter().enumerate() {
            let placeholder = format!("%{}", i + 1);
            if i == 0 && val.contains(' ') {
                r = replace_all(&r, &placeholder, &format!("\"{}\"", val));
            } else {
                r = replace_all(&r, &placeholder, val);
            }
        }
    }

    if opts.confirm {
        let co = use_color(1);
        print!(
            "{}Execute:{} {} {}[y/N]{} ",
            if co { "\x1b[1;33m" } else { "" },
            if co { "\x1b[0m" } else { "" },
            r,
            if co { "\x1b[1m" } else { "" },
            if co { "\x1b[0m" } else { "" }
        );
        io::stdout().flush().ok();
        let mut answer = String::new();
        io::stdin().lock().read_line(&mut answer).ok();
        let answer = answer.trim();
        if answer != "y" && answer != "Y" {
            return true;
        }
    }

    if opts.verbose || opts.dry_run {
        if use_color(1) {
            println!("\x1b[36m{}\x1b[0m", r);
        } else {
            println!("{}", r);
        }
    }

    if opts.dry_run {
        stats.commands_run += 1;
        return true;
    }

    if opts.jobs > 1 {
        wait_for_slot(opts, stats);
        if STOP_REQUESTED.load(Ordering::SeqCst) {
            return false;
        }
        match unsafe { nix::unistd::fork() } {
            Ok(nix::unistd::ForkResult::Child) => {
                let c_sh = CString::new(opts.shell.as_str()).unwrap();
                let c_sh_arg = CString::new(opts.shell_name.as_str()).unwrap();
                let c_c = CString::new("-c").unwrap();
                let c_cmd = CString::new(r.as_str()).unwrap();
                nix::unistd::execv(
                    &c_sh,
                    &[c_sh_arg.as_c_str(), c_c.as_c_str(), c_cmd.as_c_str()],
                )
                .ok();
                unsafe { libc::_exit(127) };
            }
            Ok(nix::unistd::ForkResult::Parent { child }) => {
                CHILD_PIDS.lock().unwrap().push(child);
            }
            Err(e) => {
                eprintln!("fork: {}", e);
                stats.commands_failed += 1;
                return !opts.stop_on_error;
            }
        }
        return true;
    }

    let ret = system_cmd(&r, opts);
    stats.commands_run += 1;
    if INTERRUPTED.load(Ordering::SeqCst) {
        return false;
    }
    if ret != 0 {
        stats.commands_failed += 1;
        if opts.stop_on_error {
            error!("command failed (exit {}), stopping.", ret);
            STOP_REQUESTED.store(true, Ordering::SeqCst);
            return false;
        }
    }
    true
}

fn wait_for_slot(opts: &Options, stats: &mut Stats) {
    loop {
        let len = CHILD_PIDS.lock().unwrap().len() as i32;
        if len < opts.jobs {
            break;
        }
        match nix::sys::wait::wait() {
            Ok(ws) => {
                let pid = match ws {
                    nix::sys::wait::WaitStatus::Exited(pid, _) => pid,
                    nix::sys::wait::WaitStatus::Signaled(pid, _, _) => pid,
                    _ => continue,
                };
                CHILD_PIDS.lock().unwrap().retain(|&p| p != pid);
                stats.commands_run += 1;
                let success = matches!(ws, nix::sys::wait::WaitStatus::Exited(_, 0));
                if !success {
                    stats.commands_failed += 1;
                    if opts.stop_on_error {
                        STOP_REQUESTED.store(true, Ordering::SeqCst);
                    }
                }
            }
            Err(_) => break,
        }
    }
}

fn wait_all(stats: &mut Stats) {
    loop {
        if CHILD_PIDS.lock().unwrap().is_empty() {
            break;
        }
        match nix::sys::wait::wait() {
            Ok(ws) => {
                let pid = match ws {
                    nix::sys::wait::WaitStatus::Exited(pid, _) => pid,
                    nix::sys::wait::WaitStatus::Signaled(pid, _, _) => pid,
                    _ => continue,
                };
                CHILD_PIDS.lock().unwrap().retain(|&p| p != pid);
                stats.commands_run += 1;
                let success = matches!(ws, nix::sys::wait::WaitStatus::Exited(_, 0));
                if !success {
                    stats.commands_failed += 1;
                }
            }
            Err(_) => break,
        }
    }
}

/// Recursively walk a directory and collect all matching file paths into `files`.
///
/// This is the list-all counterpart to [`add_directory()`]. Instead of executing
/// a command for each match, it appends the full path of every matched entry to
/// the `files` vector. The caller then joins these paths and invokes the command
/// template once via [`proc_cmd()`] with `%0` expanded to the entire list.
///
/// # Arguments
///
/// - `path` — the directory to scan
/// - `regex` — compiled regex matched against each entry's full path
/// - `exclude_regex` — optional compiled exclude pattern
/// - `expr_root` — optional parsed expression tree (from `--expr`)
/// - `opts` — runtime options (depth, hidden, filters, etc.)
/// - `stats` — mutable execution statistics (files_matched is incremented)
/// - `files` — accumulator for matched file paths
/// - `depth` — current recursion depth (0 at the root call)
fn fill_list(
    path: &Path,
    regex: &Regex,
    exclude_regex: Option<&Regex>,
    expr_root: Option<&ExprNode>,
    opts: &Options,
    stats: &mut Stats,
    files: &mut Vec<String>,
    depth: i32,
) {
    if opts.max_depth >= 0 && depth > opts.max_depth {
        return;
    }
    if STOP_REQUESTED.load(Ordering::SeqCst) || INTERRUPTED.load(Ordering::SeqCst) {
        return;
    }

    let entries = match fs::read_dir(path) {
        Ok(e) => e,
        Err(e) => {
            error!("could not open directory: {}: {}", path.display(), e);
            process::exit(1);
        }
    };

    for entry in entries {
        if STOP_REQUESTED.load(Ordering::SeqCst) || INTERRUPTED.load(Ordering::SeqCst) {
            return;
        }
        let entry = match entry {
            Ok(e) => e,
            Err(_) => continue,
        };

        let filename = entry.file_name().to_string_lossy().to_string();

        // Skip hidden files unless --all
        if !opts.hidden && filename.starts_with('.') {
            continue;
        }

        // Exclude pattern check
        if let Some(excl) = exclude_regex {
            if excl.is_match(&filename) {
                continue;
            }
        }

        let symlink_meta = match entry.path().symlink_metadata() {
            Ok(m) => m,
            Err(_) => continue,
        };

        let is_symlink = symlink_meta.file_type().is_symlink();
        let meta = if is_symlink && opts.type_filter != 'l' {
            match entry.path().metadata() {
                Ok(m) => m,
                Err(_) => continue,
            }
        } else {
            symlink_meta.clone()
        };

        let is_dir = meta.is_dir();
        let is_file = meta.is_file();

        if is_dir && !is_symlink {
            if opts.type_filter == 'd' {
                let fullpath = entry.path().to_string_lossy().to_string();
                if entry_matches_path(&fullpath, regex, expr_root) && matches_filters(&entry.path(), &meta, opts) {
                    stats.files_matched += 1;
                    files.push(fullpath);
                }
            }
            fill_list(
                &entry.path(),
                regex,
                exclude_regex,
                expr_root,
                opts,
                stats,
                files,
                depth + 1,
            );
        } else if is_symlink && opts.type_filter == 'l' {
            let fullpath = entry.path().to_string_lossy().to_string();
            if entry_matches_path(&fullpath, regex, expr_root) && matches_filters(&entry.path(), &symlink_meta, opts) {
                stats.files_matched += 1;
                files.push(fullpath);
            }
        } else if is_file || (is_symlink && opts.type_filter == '\0') {
            let fullpath = entry.path().to_string_lossy().to_string();
            if entry_matches_path(&fullpath, regex, expr_root) && matches_filters(&entry.path(), &meta, opts) {
                stats.files_matched += 1;
                files.push(fullpath);
            }
        }
    }
}

fn add_directory(
    path: &Path,
    cmd: &str,
    regex: &Regex,
    exclude_regex: Option<&Regex>,
    expr_root: Option<&ExprNode>,
    args: &mut Vec<String>,
    opts: &Options,
    stats: &mut Stats,
    depth: i32,
) {
    // Respect max depth: if we've exceeded the limit, stop recursing.
    if opts.max_depth >= 0 && depth > opts.max_depth {
        return;
    }
    // If a command previously failed and --stop-on-error was set, bail out.
    if STOP_REQUESTED.load(Ordering::SeqCst) || INTERRUPTED.load(Ordering::SeqCst) {
        return;
    }

    // Open the directory for iteration. Exit on failure (matches C++ behavior).
    let entries = match fs::read_dir(path) {
        Ok(e) => e,
        Err(e) => {
            error!("could not open directory: {}: {}", path.display(), e);
            process::exit(1);
        }
    };

    for entry in entries {
        if STOP_REQUESTED.load(Ordering::SeqCst) || INTERRUPTED.load(Ordering::SeqCst) {
            return;
        }
        let entry = match entry {
            Ok(e) => e,
            Err(_) => continue,
        };

        let filename = entry.file_name().to_string_lossy().to_string();

        // Skip hidden files unless --all
        if !opts.hidden && filename.starts_with('.') {
            continue;
        }

        // Exclude pattern check
        if let Some(excl) = exclude_regex {
            if excl.is_match(&filename) {
                continue;
            }
        }

        // Use symlink_metadata first so we can detect symlinks without
        // following them. This is critical for the --type l filter.
        let symlink_meta = match entry.path().symlink_metadata() {
            Ok(m) => m,
            Err(_) => continue,
        };

        let is_symlink = symlink_meta.file_type().is_symlink();
        // For symlinks with type filter != 'l', resolve the symlink to get
        // the target's real metadata (is it a file or directory?).
        // For type filter == 'l', keep the symlink metadata as-is.
        let meta = if is_symlink && opts.type_filter != 'l' {
            match entry.path().metadata() {
                Ok(m) => m,
                Err(_) => continue,
            }
        } else {
            symlink_meta.clone()
        };

        let is_dir = meta.is_dir();
        let is_file = meta.is_file();

        // Entry classification and processing:
        // - Real directories (not symlinks) are recursed into. If --type d is
        //   active, they are also tested against the regex for matching.
        // - Symlinks with --type l are tested against the regex.
        // - Regular files (and unfiltered symlinks) are tested against the regex.
        if is_dir && !is_symlink {
            // If type filter is 'd', also match directories against regex
            if opts.type_filter == 'd' {
                let fullpath = entry.path().to_string_lossy().to_string();
                if entry_matches_path(&fullpath, regex, expr_root) && matches_filters(&entry.path(), &meta, opts) {
                    stats.files_matched += 1;
                    args[0] = fullpath;
                    if !proc_cmd(cmd, args, None, opts, stats) {
                        return;
                    }
                }
            }
            add_directory(
                &entry.path(),
                cmd,
                regex,
                exclude_regex,
                expr_root,
                args,
                opts,
                stats,
                depth + 1,
            );
        } else if is_symlink && opts.type_filter == 'l' {
            let fullpath = entry.path().to_string_lossy().to_string();
            if entry_matches_path(&fullpath, regex, expr_root) && matches_filters(&entry.path(), &symlink_meta, opts) {
                stats.files_matched += 1;
                args[0] = fullpath;
                if !proc_cmd(cmd, args, None, opts, stats) {
                    return;
                }
            }
        } else if is_file || (is_symlink && opts.type_filter == '\0') {
            let fullpath = entry.path().to_string_lossy().to_string();
            if entry_matches_path(&fullpath, regex, expr_root) && matches_filters(&entry.path(), &meta, opts) {
                stats.files_matched += 1;
                args[0] = fullpath;
                if !proc_cmd(cmd, args, None, opts, stats) {
                    return;
                }
            }
        }
    }
}

/// SIGINT handler — sets the INTERRUPTED flag for clean exit.
extern "C" fn sigint_handler(_sig: libc::c_int) {
    INTERRUPTED.store(true, Ordering::SeqCst);
}

/// Program entry point.
///
/// Parses command-line arguments via `clap`, validates positional arguments
/// and placeholder consistency, constructs [`Options`], compiles regex patterns
/// (anchoring with `^(?:...)$` when `--regex-match` is active), runs the
/// recursive directory traversal via [`add_directory()`] (or [`fill_list()`]
/// in `--list-all` mode), waits for parallel children if applicable, and prints
/// a summary.
///
/// # Exit codes
///
/// - `0` — all commands succeeded (or dry-run completed)
/// - `1` — at least one command failed, or invalid arguments were provided
/// Print usage/help with colored output (matches C++ shell-cmd format).
fn print_help() {
    let co = use_color(1);
    let (b, bw, bc, by, g, r) = if co {
        (
            "\x1b[1m",
            "\x1b[1;37m",
            "\x1b[1;36m",
            "\x1b[1;33m",
            "\x1b[32m",
            "\x1b[0m",
        )
    } else {
        ("", "", "", "", "", "")
    };
    println!(
        "\
{b}usage:{r} {bw}shell-cmd-rs{r} [options] path \"command %1 [%2 %3..]\" regex [extra_args..]

{bw}Recursively find files matching regex and run command for each.{r}
{bc}(Rust implementation of shell-cmd){r}

{by}placeholders:{r}
  {g}%0{r}          filename only (no path, per-match mode)
  {g}%1{r}          full path to matched file
  {g}%2+{r}         extra arguments from command line
  {g}%b{r}          basename without extension
  {g}%e{r}          file extension (including dot)

  (with -l/--list-all) %0 expands to all matched paths joined by spaces

{by}options:{r}
  {g}-n, --dry-run{r}       dry-run, print commands without executing
  {g}-v, --verbose{r}       verbose, print each command before running
  {g}-a, --all{r}           include hidden files/directories
  {g}-l, --list-all{r}      collect all matches and invoke command once with %0=all-matches
  {g}-d, --depth N{r}       max recursion depth (0 = current dir only)
  {g}-s, --size SIZE{r}     filter by size: +10M (>10MB), -1K (<1KB),
                      4096 (exactly 4096 bytes). Suffixes: K, M, G
  {g}-m, --mtime DAYS{r}    filter by modification time: +7 (older than 7 days),
                      -1 (modified within last day), 3 (exactly 3 days)
  {g}-p, --perm MODE{r}     filter by permissions (octal), e.g. 755
  {g}-u, --user USER{r}     filter by owner username
  {g}-g, --group GROUP{r}   filter by group name
  {g}-t, --type TYPE{r}     filter by type: f (file), d (directory), l (symlink)
  {g}-x, --exclude REGEX{r} exclude files/directories matching REGEX
  {g}-i, --glob-exclude{r}  treat exclude pattern as a glob instead of regex
  {g}-e, --stop-on-error{r} stop on first command failure
  {g}-c, --confirm{r}       prompt for confirmation before each command
  {g}-j, --jobs N{r}        run N commands in parallel (default: 1)
  {g}-w, --shell SHELL{r}   shell to use for execution (default: /bin/bash)
  {g}-b, --glob{r}          treat pattern as a glob (*, ?) instead of regex
  {g}-z, --regex-match{r}   use regex-match (full path must match) instead of search
  {g}-f, --expr EXPR{r}     expression filter: compose glob(), regex(), regex_match()
                      with and/or/not and parentheses
  {g}-h, --help{r}          show this help

{by}regex modes:{r}
  By default, the regex is tested as a {g}substring search{r} (matches anywhere
  in the path). With {g}-z{r}/{g}--regex-match{r}, the entire path must match the
  pattern (equivalent to anchoring with ^...$).

{by}glob mode:{r}
  With {g}-b{r}/{g}--glob{r}, write familiar wildcard patterns instead of regex:
  {g}*{r} matches anything, {g}?{r} matches a single character, and regex-special
  characters ({g}.{r}, {g}+{r}, {g}({r}, etc.) are auto-escaped.

{by}expr mode:{r}
  With {g}-f{r}/{g}--expr{r}, compose filter functions with boolean operators:
  {g}glob(\"pattern\"){r}, {g}regex(\"pattern\"){r}, {g}regex_match(\"pattern\"){r}
  combined with {g}and{r}, {g}or{r}, {g}not{r}, and parentheses.
  When --expr is used, the regex positional argument is not required.",
        b = b,
        bw = bw,
        bc = bc,
        by = by,
        g = g,
        r = r
    );
}

fn main() {
    // Install SIGINT handler for clean Ctrl+C exit
    unsafe {
        let sa = nix::sys::signal::SigAction::new(
            nix::sys::signal::SigHandler::Handler(sigint_handler),
            nix::sys::signal::SaFlags::empty(),
            nix::sys::signal::SigSet::empty(),
        );
        let _ = nix::sys::signal::sigaction(nix::sys::signal::Signal::SIGINT, &sa);
    }

    // If no arguments provided, print colored help and exit (matches C++ behavior)
    if std::env::args().len() == 1 {
        print_help();
        process::exit(0);
    }

    let cli = Cli::parse();

    // Validate type filter
    if let Some(t) = cli.type_filter {
        if t != 'f' && t != 'd' && t != 'l' {
            error!(
                "invalid type '{}'. Use f (file), d (directory), or l (symlink).",
                t
            );
            process::exit(1);
        }
    }

    let opts = Options {
        dry_run: cli.dry_run,
        verbose: cli.verbose,
        hidden: cli.all,
        max_depth: cli.depth.unwrap_or(-1),
        size_filter: cli
            .size
            .as_ref()
            .map(|s| parse_size_filter(s))
            .unwrap_or(SizeFilter {
                active: false,
                op: CmpOp::Eq,
                bytes: 0,
            }),
        mtime_filter: cli
            .mtime
            .as_ref()
            .map(|s| parse_time_filter(s))
            .unwrap_or(TimeFilter {
                active: false,
                op: CmpOp::Eq,
                days: 0,
            }),
        perm_filter: cli.perm.unwrap_or_default(),
        user_filter: cli.user.unwrap_or_default(),
        group_filter: cli.group.unwrap_or_default(),
        type_filter: cli.type_filter.unwrap_or('\0'),
        exclude_pattern: cli.exclude.clone().unwrap_or_default(),
        stop_on_error: cli.stop_on_error,
        confirm: cli.confirm,
        jobs: cli.jobs.max(1),
        shell_name: cli
            .shell
            .rsplit('/')
            .next()
            .unwrap_or(&cli.shell)
            .to_string(),
        shell: cli.shell,
        collect_all: cli.list_all,
        glob: cli.glob,
        regex_match: cli.regex_match,
        glob_exclude: cli.glob_exclude,
        expr_str: cli.expr.clone().unwrap_or_default(),
    };

    // Parse expression filter if --expr was provided
    let expr_root: Option<Box<ExprNode>> = if !opts.expr_str.is_empty() {
        Some(ExprParser::new(&opts.expr_str).parse())
    } else {
        None
    };

    let positional = &cli.args;
    let min_args = if expr_root.is_some() { 2 } else { 3 };
    if positional.len() < min_args {
        if expr_root.is_some() {
            error!("at least two positional arguments required when --expr is used.");
        } else {
            error!("at least three positional arguments required.");
        }
        print_help();
        process::exit(1);
    }

    let path = PathBuf::from(&positional[0]);
    let input = &positional[1];

    // When --expr is used, the regex positional is optional.
    // Use a dummy "match-nothing" regex as placeholder when no pattern is given.
    let regex_str = if positional.len() >= 3 {
        if opts.glob {
            glob_to_regex(&positional[2])
        } else {
            positional[2].clone()
        }
    } else {
        // --expr mode without regex arg: use match-everything pattern
        ".*".to_string()
    };

    // In regex-match mode, anchor the pattern so it must match the entire path.
    // Wrapping in ^(?:...)$ converts a substring search into a full-string match,
    // equivalent to C++ std::regex_match vs std::regex_search.
    let regex_str = if opts.regex_match {
        format!("^(?:{})$", regex_str)
    } else {
        regex_str
    };

    let regex = Regex::new(&regex_str).unwrap_or_else(|e| {
        error!("invalid regex '{}': {}", regex_str, e);
        process::exit(1);
    });

    let exclude_pattern = if opts.glob_exclude && !opts.exclude_pattern.is_empty() {
        glob_to_regex(&opts.exclude_pattern)
    } else {
        opts.exclude_pattern.clone()
    };

    // Anchor the exclude pattern in regex-match mode as well.
    let exclude_pattern = if opts.regex_match && !exclude_pattern.is_empty() {
        format!("^(?:{})$", exclude_pattern)
    } else {
        exclude_pattern
    };

    let exclude_regex = if !exclude_pattern.is_empty() {
        Some(Regex::new(&exclude_pattern).unwrap_or_else(|e| {
            error!("invalid exclude regex '{}': {}", exclude_pattern, e);
            process::exit(1);
        }))
    } else {
        None
    };

    // In --list-all mode, the placeholder index starts at 1 (no per-file %1)
    // and args does not include a "filename" placeholder entry.
    let extra_start = if positional.len() >= 3 { 3 } else { 2 };
    let mut index: usize = if opts.collect_all { 1 } else { 2 };
    let mut args: Vec<String> = if opts.collect_all {
        Vec::new()
    } else {
        vec!["filename".to_string()]
    };
    for i in extra_start..positional.len() {
        let placeholder = format!("%{}", index);
        if !input.contains(&placeholder) {
            error!(
                "command has no placeholder %{} for extra argument \"{}\"",
                index, positional[i]
            );
            process::exit(1);
        }
        args.push(positional[i].clone());
        index += 1;
    }

    let mut stats = Stats {
        files_matched: 0,
        commands_run: 0,
        commands_failed: 0,
    };

    if opts.collect_all {
        // --list-all mode: collect all matching paths, then run the command once
        // with %0 expanded to the space-joined list of all matches.
        let mut files: Vec<String> = Vec::new();
        fill_list(
            &path,
            &regex,
            exclude_regex.as_ref(),
            expr_root.as_deref(),
            &opts,
            &mut stats,
            &mut files,
            0,
        );
        let all_files = files.join(" ");
        if proc_cmd(input, &args, Some(&all_files), &opts, &mut stats) {
            if opts.verbose {
                println!("Success command file list: {} .", all_files);
            }
            process::exit(0);
        } else {
            println!("List all command failed.");
            process::exit(1);
        }
    }

    add_directory(
        &path,
        input,
        &regex,
        exclude_regex.as_ref(),
        expr_root.as_deref(),
        &mut args,
        &opts,
        &mut stats,
        0,
    );

    if opts.jobs > 1 {
        wait_all(&mut stats);
    }

    if INTERRUPTED.load(Ordering::SeqCst) {
        // Kill outstanding child processes
        {
            let pids = CHILD_PIDS.lock().unwrap();
            for &pid in pids.iter() {
                let _ = nix::sys::signal::kill(pid, nix::sys::signal::Signal::SIGTERM);
            }
        }
        // Wait for them to finish
        loop {
            if CHILD_PIDS.lock().unwrap().is_empty() {
                break;
            }
            match nix::sys::wait::wait() {
                Ok(ws) => {
                    let pid = match ws {
                        nix::sys::wait::WaitStatus::Exited(pid, _) => pid,
                        nix::sys::wait::WaitStatus::Signaled(pid, _, _) => pid,
                        _ => continue,
                    };
                    CHILD_PIDS.lock().unwrap().retain(|&p| p != pid);
                }
                Err(_) => break,
            }
        }
        eprintln!("\nInterrupted.");
        let co = use_color(2);
        if stats.commands_run > 0 || stats.commands_failed > 0 {
            if co {
                eprintln!(
                    "\x1b[1mSummary:\x1b[0m \x1b[1;32m{}\x1b[0m matched, \x1b[1;33m{}\x1b[0m run, {}{}\x1b[0m failed",
                    stats.files_matched,
                    stats.commands_run,
                    if stats.commands_failed > 0 { "\x1b[1;31m" } else { "\x1b[1;32m" },
                    stats.commands_failed
                );
            } else {
                eprintln!(
                    "Summary: {} matched, {} run, {} failed",
                    stats.files_matched, stats.commands_run, stats.commands_failed
                );
            }
        }
        process::exit(130);
    }

    if opts.verbose || opts.dry_run || stats.commands_failed > 0 {
        let co = use_color(2);
        if co {
            eprintln!(
                "\n\x1b[1mSummary:\x1b[0m \x1b[1;32m{}\x1b[0m matched, \x1b[1;33m{}\x1b[0m run, {}{}\x1b[0m failed",
                stats.files_matched,
                stats.commands_run,
                if stats.commands_failed > 0 { "\x1b[1;31m" } else { "\x1b[1;32m" },
                stats.commands_failed
            );
        } else {
            eprintln!(
                "\nSummary: {} matched, {} run, {} failed",
                stats.files_matched, stats.commands_run, stats.commands_failed
            );
        }
    }

    if stats.commands_failed > 0 {
        process::exit(1);
    }
}
