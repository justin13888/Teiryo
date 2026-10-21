//! What `teiryo -V` and `teiryod -V` print, and how both read their argv.
//!
//! Both binaries are installed from source, and the daemon outlives the TUI
//! across upgrades, so the crate version alone cannot say which build is
//! running: every build of a checkout is `0.1.0`. The version text also carries
//! the commit, the profile, the compiler, the build time and the wire protocol
//! version, captured by `build_info.rs` beside this crate (each binary's build
//! script) and read with [`build_info!`].

use std::ffi::OsString;

use crate::protocol::handshake::PROTOCOL_VERSION;

/// Build provenance of one binary. Each fact is `unknown` when the build could
/// not establish it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BuildInfo {
    /// The crate version, `CARGO_PKG_VERSION`.
    pub version: &'static str,
    /// The commit built from, `-dirty` when the tree had uncommitted changes.
    pub commit: &'static str,
    /// The committer date of that commit, ISO 8601.
    pub commit_date: &'static str,
    /// The Cargo profile.
    pub profile: &'static str,
    /// The compiler's own `--version` line.
    pub rustc: &'static str,
    /// When the build ran, in seconds since the Unix epoch.
    pub build_epoch: &'static str,
}

/// The calling binary's [`BuildInfo`]. A macro rather than a function because
/// `env!` must expand in the binary crate, whose build script set the values.
#[macro_export]
macro_rules! build_info {
    () => {
        $crate::version::BuildInfo {
            version: env!("CARGO_PKG_VERSION"),
            commit: env!("TEIRYO_COMMIT_HASH"),
            commit_date: env!("TEIRYO_COMMIT_DATE"),
            profile: env!("TEIRYO_BUILD_PROFILE"),
            rustc: env!("TEIRYO_RUSTC_VERSION"),
            build_epoch: env!("TEIRYO_BUILD_EPOCH"),
        }
    };
}

/// The version text for both `-V` and `--version`, without a trailing newline.
pub fn long_version(name: &str, info: &BuildInfo) -> String {
    format!(
        "{name} {}\n\
         commit:   {} ({})\n\
         profile:  {}\n\
         rustc:    {}\n\
         built:    {}\n\
         protocol: v{PROTOCOL_VERSION}",
        info.version,
        info.commit,
        info.commit_date,
        info.profile,
        info.rustc,
        built(info.build_epoch),
    )
}

/// `epoch` seconds as a UTC `YYYY-MM-DDTHH:MM:SSZ`, or `unknown` when it is not
/// a representable count of seconds.
fn built(epoch: &str) -> String {
    epoch
        .parse::<i64>()
        .ok()
        .and_then(|secs| chrono::DateTime::from_timestamp(secs, 0))
        .map_or_else(
            || "unknown".to_owned(),
            |at| at.format("%Y-%m-%dT%H:%M:%SZ").to_string(),
        )
}

/// What the command line asks a binary to do. Neither binary takes operands,
/// so anything but a recognised flag is a usage error — rather than being
/// ignored, which would start a daemon or take over the terminal for a typo.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Invocation {
    /// No arguments: the binary's normal job.
    Run,
    /// `-h` / `--help`.
    Help,
    /// `-V` / `--version`.
    Version,
    /// One of the binary's own [`Flag`]s, by its long form.
    Flag(&'static str),
    /// The first argument nothing recognises, lossily decoded for display.
    Unexpected(String),
}

/// A long-only flag one binary accepts beyond `-h` and `-V`, listed in its
/// `--help`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Flag {
    /// The flag as typed, e.g. `--print-socket-path`.
    pub long: &'static str,
    /// Its line in `--help`.
    pub help: &'static str,
}

/// What `main` does once [`handle_args_with`] has read the command line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// No arguments: the binary's normal job.
    Run,
    /// Leave with this status; the output is already printed.
    Exit(i32),
    /// Act on this [`Flag`], by its long form.
    Flag(&'static str),
}

/// Classify `args`, excluding the program name. Help wins over version, as it
/// does in most CLIs, but only once every argument has been recognised.
pub fn parse_args<I>(args: I) -> Invocation
where
    I: IntoIterator<Item = OsString>,
{
    parse_args_with(args, &[])
}

/// [`parse_args`] for a binary that also accepts `flags`. Help and version
/// both win over a binary's own flag, and the first of those given wins.
pub fn parse_args_with<I>(args: I, flags: &[Flag]) -> Invocation
where
    I: IntoIterator<Item = OsString>,
{
    let mut help = false;
    let mut version = false;
    let mut own = None;
    for arg in args {
        let flag = arg
            .to_str()
            .and_then(|s| flags.iter().find(|f| f.long == s));
        match (arg.to_str(), flag) {
            (Some("-h" | "--help"), _) => help = true,
            (Some("-V" | "--version"), _) => version = true,
            (_, Some(flag)) => {
                own.get_or_insert(flag.long);
            }
            _ => return Invocation::Unexpected(arg.to_string_lossy().into_owned()),
        }
    }
    if help {
        Invocation::Help
    } else if version {
        Invocation::Version
    } else if let Some(long) = own {
        Invocation::Flag(long)
    } else {
        Invocation::Run
    }
}

/// Act on the process's argv before the binary does anything else: print the
/// help, the version or a usage error, and return the exit code to leave with.
/// `None` means there were no arguments and the binary should do its job.
///
/// Called first in both `main`s, so `-V` never binds or connects to the
/// daemon socket and never takes over the terminal.
pub fn handle_args(name: &str, about: &str, info: &BuildInfo) -> Option<i32> {
    match handle_args_with(name, about, info, &[]) {
        Action::Exit(code) => Some(code),
        // No flags were offered, so none can come back.
        Action::Run | Action::Flag(_) => None,
    }
}

/// [`handle_args`] for a binary that also accepts `flags`, which `--help`
/// lists. A flag given is returned for the binary to act on.
pub fn handle_args_with(name: &str, about: &str, info: &BuildInfo, flags: &[Flag]) -> Action {
    match parse_args_with(std::env::args_os().skip(1), flags) {
        Invocation::Run => Action::Run,
        Invocation::Flag(long) => Action::Flag(long),
        Invocation::Help => {
            println!("{}", help_with(name, about, info, flags));
            Action::Exit(0)
        }
        Invocation::Version => {
            println!("{}", long_version(name, info));
            Action::Exit(0)
        }
        Invocation::Unexpected(arg) => {
            eprintln!("{}", usage_error(name, &arg));
            Action::Exit(2)
        }
    }
}

/// The `--help` text, without a trailing newline.
pub fn help(name: &str, about: &str, info: &BuildInfo) -> String {
    help_with(name, about, info, &[])
}

/// [`help`] listing `flags` after `-h` and `-V`, every description aligned
/// two columns past the widest flag.
pub fn help_with(name: &str, about: &str, info: &BuildInfo, flags: &[Flag]) -> String {
    let own: Vec<(String, &str)> = flags
        .iter()
        .map(|f| (format!("    {}", f.long), f.help))
        .collect();
    let rows: Vec<(&str, &str)> = [
        ("-h, --help", "Print help"),
        ("-V, --version", "Print version and build information"),
    ]
    .into_iter()
    .chain(own.iter().map(|(flag, text)| (flag.as_str(), *text)))
    .collect();
    let width = rows.iter().map(|(flag, _)| flag.len()).max().unwrap_or(0) + 2;
    let options: Vec<String> = rows
        .iter()
        .map(|(flag, text)| format!("  {flag:<width$}{text}"))
        .collect();
    format!(
        "{name} {}\n\
         {about}\n\n\
         Usage: {name} [OPTIONS]\n\n\
         Options:\n{}",
        info.version,
        options.join("\n")
    )
}

/// The usage error for an argument nothing recognises, without a trailing
/// newline. Printed to stderr, with exit status 2.
pub fn usage_error(name: &str, arg: &str) -> String {
    format!(
        "{name}: unexpected argument '{arg}'\n\n\
         Usage: {name} [OPTIONS]\n\n\
         For more information, try '--help'."
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: BuildInfo = BuildInfo {
        version: "0.1.0",
        commit: "f78c2ce1a2b3",
        commit_date: "2026-10-07T15:51:43-04:00",
        profile: "release",
        rustc: "rustc 1.99.1 (58cf8f9c3 2026-10-13)",
        build_epoch: "1792353342",
    };

    fn args(list: &[&str]) -> Vec<OsString> {
        list.iter().map(OsString::from).collect()
    }

    #[test]
    fn long_version_layout() {
        assert_eq!(
            long_version("teiryo", &SAMPLE),
            format!(
                "teiryo 0.1.0\n\
                 commit:   f78c2ce1a2b3 (2026-10-07T15:51:43-04:00)\n\
                 profile:  release\n\
                 rustc:    rustc 1.99.1 (58cf8f9c3 2026-10-13)\n\
                 built:    2026-10-18T19:55:42Z\n\
                 protocol: v{PROTOCOL_VERSION}"
            )
        );
    }

    /// Each fact the build could not establish still yields a complete layout,
    /// so a user reporting a problem pastes the same shape either way.
    #[test]
    fn unknown_facts_keep_the_layout() {
        let unknown = BuildInfo {
            version: "0.1.0",
            commit: "unknown",
            commit_date: "unknown",
            profile: "unknown",
            rustc: "unknown",
            build_epoch: "unknown",
        };
        let text = long_version("teiryod", &unknown);
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 6, "{text}");
        assert_eq!(lines[0], "teiryod 0.1.0");
        assert_eq!(lines[1], "commit:   unknown (unknown)");
        assert_eq!(lines[4], "built:    unknown");
    }

    #[test]
    fn built_rejects_what_is_not_a_timestamp() {
        assert_eq!(built("0"), "1970-01-01T00:00:00Z");
        assert_eq!(built("-1"), "1969-12-31T23:59:59Z");
        assert_eq!(built(""), "unknown");
        assert_eq!(built("soon"), "unknown");
        assert_eq!(built(&i64::MAX.to_string()), "unknown");
    }

    #[test]
    fn no_arguments_runs() {
        assert_eq!(parse_args(args(&[])), Invocation::Run);
    }

    #[test]
    fn version_and_help_flags() {
        assert_eq!(parse_args(args(&["-V"])), Invocation::Version);
        assert_eq!(parse_args(args(&["--version"])), Invocation::Version);
        assert_eq!(parse_args(args(&["-h"])), Invocation::Help);
        assert_eq!(parse_args(args(&["--help"])), Invocation::Help);
        assert_eq!(parse_args(args(&["-V", "--help"])), Invocation::Help);
    }

    /// The falsifier for the flags above: an unrecognised argument anywhere is
    /// an error, even after a valid flag, and lowercase `-v` is not `-V`.
    #[test]
    fn anything_else_is_unexpected() {
        assert_eq!(
            parse_args(args(&["--bogus"])),
            Invocation::Unexpected("--bogus".into())
        );
        assert_eq!(
            parse_args(args(&["-V", "status"])),
            Invocation::Unexpected("status".into())
        );
        assert_eq!(
            parse_args(args(&["-v"])),
            Invocation::Unexpected("-v".into())
        );
    }

    #[test]
    fn help_lists_both_flags() {
        let text = help("teiryod", "Teiryo daemon.", &SAMPLE);
        assert_eq!(
            text,
            "teiryod 0.1.0\n\
             Teiryo daemon.\n\n\
             Usage: teiryod [OPTIONS]\n\n\
             Options:\n  \
               -h, --help     Print help\n  \
               -V, --version  Print version and build information"
        );
    }

    const PRINT: Flag = Flag {
        long: "--print-socket-path",
        help: "Print the socket path and exit",
    };

    #[test]
    fn own_flags_parse_only_where_offered() {
        assert_eq!(
            parse_args_with(args(&["--print-socket-path"]), &[PRINT]),
            Invocation::Flag("--print-socket-path")
        );
        assert_eq!(
            parse_args(args(&["--print-socket-path"])),
            Invocation::Unexpected("--print-socket-path".into()),
            "a flag one binary offers is a typo to the other"
        );
        assert_eq!(
            parse_args_with(args(&["--print-socket-path", "-V"]), &[PRINT]),
            Invocation::Version
        );
        assert_eq!(
            parse_args_with(args(&["--print-socket-path", "--help"]), &[PRINT]),
            Invocation::Help
        );
        assert_eq!(
            parse_args_with(args(&["--print-socket-path", "x"]), &[PRINT]),
            Invocation::Unexpected("x".into())
        );
    }

    #[test]
    fn help_aligns_own_flags_with_the_rest() {
        let text = help_with("teiryod", "Teiryo daemon.", &SAMPLE, &[PRINT]);
        assert_eq!(
            text,
            "teiryod 0.1.0\n\
             Teiryo daemon.\n\n\
             Usage: teiryod [OPTIONS]\n\n\
             Options:\n  \
               -h, --help               Print help\n  \
               -V, --version            Print version and build information\n  \
               \x20   --print-socket-path  Print the socket path and exit"
        );
    }

    #[test]
    fn usage_error_names_the_argument_and_the_way_out() {
        let text = usage_error("teiryo", "--bogus");
        assert!(
            text.starts_with("teiryo: unexpected argument '--bogus'"),
            "{text}"
        );
        assert!(text.contains("Usage: teiryo [OPTIONS]"), "{text}");
        assert!(text.ends_with("try '--help'."), "{text}");
    }
}
