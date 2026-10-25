//! What `teiryo -V` and `teiryod -V` print, and how both read their argv.
//!
//! Teiryo is never published to crates.io, so no crate carries a version: a
//! build is named by the commit it came from. The version is that commit's
//! committer date in UTC and its hash, `YYYY.MM.DD-<sha12>`
//! ([`release_version`]), so every build of one commit reports the same one,
//! and a release is a signed tag of exactly that name. The version text also
//! carries the commit date, the profile, the compiler, the build time and the
//! wire protocol version, captured by `build_info.rs` beside this crate (each
//! binary's build script) and read with [`build_info!`].

use std::ffi::OsString;

use crate::protocol::handshake::PROTOCOL_VERSION;

/// Build provenance of one binary. Each fact is `unknown` when the build could
/// not establish it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BuildInfo {
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
            commit: env!("TEIRYO_COMMIT_HASH"),
            commit_date: env!("TEIRYO_COMMIT_DATE"),
            profile: env!("TEIRYO_BUILD_PROFILE"),
            rustc: env!("TEIRYO_RUSTC_VERSION"),
            build_epoch: env!("TEIRYO_BUILD_EPOCH"),
        }
    };
}

impl BuildInfo {
    /// This build's version; see [`release_version`].
    pub fn version(&self) -> String {
        release_version(self.commit, self.commit_date)
    }
}

/// The version of a build of `commit`, committed at `commit_date` (ISO 8601):
/// the commit date in UTC, then the commit, as `2026.10.25-91a0a7a1b2c3`. A
/// `-dirty` commit keeps its suffix. An `unknown` commit is version `unknown`,
/// and a commit whose date is not known is its own version.
///
/// A release tag is named by the same rule (`mise run release`), and the
/// release workflow checks each binary's `-V` against the tag, so the two
/// cannot drift apart unnoticed.
pub fn release_version(commit: &str, commit_date: &str) -> String {
    if commit == "unknown" {
        return commit.to_owned();
    }
    match chrono::DateTime::parse_from_rfc3339(commit_date) {
        Ok(at) => format!("{}-{commit}", at.to_utc().format("%Y.%m.%d")),
        Err(_) => commit.to_owned(),
    }
}

/// Whether two builds, named by their `TEIRYO_COMMIT_HASH`, are the same
/// build. Only an exact, clean, known commit can vouch for that: a `-dirty`
/// tree may differ from its commit in anything, and `unknown` says nothing, so
/// neither matches even itself.
pub fn builds_match(client: &str, daemon: &str) -> bool {
    client == daemon && !client.ends_with("-dirty") && client != "unknown"
}

/// The client and daemon commits, as `client / daemon`, when
/// [`builds_match`] cannot vouch for them; `None` when it can.
pub fn build_mismatch(client: &str, daemon: &str) -> Option<String> {
    (!builds_match(client, daemon)).then(|| format!("{client} / {daemon}"))
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
        info.version(),
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

/// What the command line asks a binary to do. Neither binary takes free
/// operands, so anything but a recognised flag (or a recognised flag's value)
/// is a usage error — rather than being ignored, which would start a daemon or
/// take over the terminal for a typo.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Invocation {
    /// No arguments: the binary's normal job.
    Run,
    /// `-h` / `--help`.
    Help,
    /// `-V` / `--version`.
    Version,
    /// The binary's own [`Flag`]s, in the order given.
    Flags(Vec<Given>),
    /// A [`Flag`] that takes a value was given last, with none after it.
    MissingValue(&'static str),
    /// The first argument nothing recognises, lossily decoded for display.
    Unexpected(String),
}

/// A long-only flag, or a bare command word such as `bridge`, that one binary
/// accepts beyond `-h` and `-V`, listed in its `--help`. A `long` that does
/// not start with `-` is listed as a command rather than an option.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Flag {
    /// The flag as typed, e.g. `--print-socket-path`.
    pub long: &'static str,
    /// The placeholder of the value it takes, e.g. `HOST`, given as the next
    /// argument or after `=`; `None` for a flag that takes none.
    pub value: Option<&'static str>,
    /// Its line in `--help`.
    pub help: &'static str,
}

/// One of the binary's own [`Flag`]s as given on the command line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Given {
    /// The flag's long form.
    pub long: &'static str,
    /// Its value, for a flag that takes one.
    pub value: Option<OsString>,
}

/// What `main` does once [`handle_args_with`] has read the command line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// No arguments: the binary's normal job.
    Run,
    /// Leave with this status; the output is already printed.
    Exit(i32),
    /// Act on these [`Flag`]s, in the order given.
    Flags(Vec<Given>),
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
/// both win over a binary's own flags, which come back in the order given. A
/// flag that takes a value consumes the next argument whatever it is, or the
/// text after `=` in `--flag=value`.
pub fn parse_args_with<I>(args: I, flags: &[Flag]) -> Invocation
where
    I: IntoIterator<Item = OsString>,
{
    let mut help = false;
    let mut version = false;
    let mut missing = None;
    let mut own = Vec::new();
    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        let text = arg.to_str();
        match text {
            Some("-h" | "--help") => {
                help = true;
                continue;
            }
            Some("-V" | "--version") => {
                version = true;
                continue;
            }
            _ => {}
        }
        if let Some(flag) = text.and_then(|s| flags.iter().find(|f| f.long == s)) {
            let value = match flag.value {
                None => None,
                Some(_) => match args.next() {
                    Some(value) => Some(value),
                    None => {
                        missing = Some(flag.long);
                        break;
                    }
                },
            };
            own.push(Given {
                long: flag.long,
                value,
            });
            continue;
        }
        let inline = text
            .and_then(|s| s.split_once('='))
            .and_then(|(name, value)| {
                let flag = flags.iter().find(|f| f.long == name && f.value.is_some())?;
                Some((flag, value))
            });
        match inline {
            Some((flag, value)) => own.push(Given {
                long: flag.long,
                value: Some(OsString::from(value)),
            }),
            None => return Invocation::Unexpected(arg.to_string_lossy().into_owned()),
        }
    }
    if help {
        Invocation::Help
    } else if version {
        Invocation::Version
    } else if let Some(long) = missing {
        Invocation::MissingValue(long)
    } else if own.is_empty() {
        Invocation::Run
    } else {
        Invocation::Flags(own)
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
        Action::Run | Action::Flags(_) => None,
    }
}

/// [`handle_args`] for a binary that also accepts `flags`, which `--help`
/// lists. The flags given are returned for the binary to act on.
pub fn handle_args_with(name: &str, about: &str, info: &BuildInfo, flags: &[Flag]) -> Action {
    match parse_args_with(std::env::args_os().skip(1), flags) {
        Invocation::Run => Action::Run,
        Invocation::Flags(given) => Action::Flags(given),
        Invocation::MissingValue(long) => {
            eprintln!("{}", missing_value_error(name, long));
            Action::Exit(2)
        }
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
/// two columns past the widest flag. Command words (a `long` without a leading
/// `-`) get a `Commands:` section of their own, before the options.
pub fn help_with(name: &str, about: &str, info: &BuildInfo, flags: &[Flag]) -> String {
    let is_command = |f: &&Flag| !f.long.starts_with('-');
    let commands: Vec<(String, &str)> = flags
        .iter()
        .filter(is_command)
        .map(|f| (f.long.to_owned(), f.help))
        .collect();
    let own: Vec<(String, &str)> = flags
        .iter()
        .filter(|f| !is_command(f))
        .map(|f| match f.value {
            Some(value) => (format!("    {} <{value}>", f.long), f.help),
            None => (format!("    {}", f.long), f.help),
        })
        .collect();
    let rows: Vec<(&str, &str)> = [
        ("-h, --help", "Print help"),
        ("-V, --version", "Print version and build information"),
    ]
    .into_iter()
    .chain(own.iter().map(|(flag, text)| (flag.as_str(), *text)))
    .collect();
    let width = rows
        .iter()
        .map(|(flag, _)| flag.len())
        .chain(commands.iter().map(|(command, _)| command.len()))
        .max()
        .unwrap_or(0)
        + 2;
    let section = |rows: &mut dyn Iterator<Item = (&str, &str)>| -> String {
        rows.map(|(flag, text)| format!("  {flag:<width$}{text}"))
            .collect::<Vec<_>>()
            .join("\n")
    };
    let (usage, commands) = if commands.is_empty() {
        ("[OPTIONS]", String::new())
    } else {
        (
            "[OPTIONS] [COMMAND]",
            format!(
                "Commands:\n{}\n\n",
                section(&mut commands.iter().map(|(c, t)| (c.as_str(), *t)))
            ),
        )
    };
    format!(
        "{name} {}\n\
         {about}\n\n\
         Usage: {name} {usage}\n\n\
         {commands}\
         Options:\n{}",
        info.version(),
        section(&mut rows.into_iter())
    )
}

/// The usage error for a flag given without the value it takes, without a
/// trailing newline. Printed to stderr, with exit status 2.
pub fn missing_value_error(name: &str, long: &str) -> String {
    format!(
        "{name}: '{long}' needs a value\n\n\
         For more information, try '--help'."
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
                "teiryo 2026.10.07-f78c2ce1a2b3\n\
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
            commit: "unknown",
            commit_date: "unknown",
            profile: "unknown",
            rustc: "unknown",
            build_epoch: "unknown",
        };
        let text = long_version("teiryod", &unknown);
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 6, "{text}");
        assert_eq!(lines[0], "teiryod unknown");
        assert_eq!(lines[1], "commit:   unknown (unknown)");
        assert_eq!(lines[4], "built:    unknown");
    }

    #[test]
    fn release_version_is_the_utc_commit_date_and_the_commit() {
        assert_eq!(
            release_version("91a0a7a1b2c3", "2026-10-25T12:00:00Z"),
            "2026.10.25-91a0a7a1b2c3"
        );
        assert_eq!(
            release_version("91a0a7a1b2c3-dirty", "2026-10-25T12:00:00Z"),
            "2026.10.25-91a0a7a1b2c3-dirty"
        );
    }

    /// The falsifier for the test above: the date is the UTC one, not the
    /// committer's local one, on either side of midnight. Otherwise the same
    /// commit would be versioned by wherever it was committed.
    #[test]
    fn release_version_dates_in_utc() {
        assert_eq!(
            release_version("91a0a7a1b2c3", "2026-10-25T22:00:00-04:00"),
            "2026.10.26-91a0a7a1b2c3"
        );
        assert_eq!(
            release_version("91a0a7a1b2c3", "2026-10-25T01:00:00+02:00"),
            "2026.10.24-91a0a7a1b2c3"
        );
    }

    #[test]
    fn release_version_without_a_commit_or_a_date() {
        assert_eq!(
            release_version("unknown", "2026-10-25T12:00:00Z"),
            "unknown"
        );
        assert_eq!(release_version("unknown", "unknown"), "unknown");
        assert_eq!(release_version("91a0a7a1b2c3", "unknown"), "91a0a7a1b2c3");
        assert_eq!(
            release_version("91a0a7a1b2c3", "2026-10-25"),
            "91a0a7a1b2c3"
        );
    }

    #[test]
    fn identical_clean_commits_match() {
        assert!(builds_match("f78c2ce1a2b3", "f78c2ce1a2b3"));
        assert_eq!(build_mismatch("f78c2ce1a2b3", "f78c2ce1a2b3"), None);
    }

    #[test]
    fn different_commits_do_not_match() {
        assert!(!builds_match("f78c2ce1a2b3", "0123456789ab"));
        assert_eq!(
            build_mismatch("f78c2ce1a2b3", "0123456789ab").as_deref(),
            Some("f78c2ce1a2b3 / 0123456789ab")
        );
    }

    /// A dirty tree never vouches for itself, on either side, even against
    /// the very same dirty commit.
    #[test]
    fn a_dirty_commit_never_matches() {
        assert!(!builds_match("abc123-dirty", "abc123-dirty"));
        assert!(!builds_match("abc123-dirty", "abc123"));
        assert!(!builds_match("abc123", "abc123-dirty"));
        assert_eq!(
            build_mismatch("abc123-dirty", "def456").as_deref(),
            Some("abc123-dirty / def456")
        );
    }

    #[test]
    fn an_unknown_commit_never_matches() {
        assert!(!builds_match("unknown", "unknown"));
        assert!(!builds_match("unknown", "abc123"));
        assert!(!builds_match("abc123", "unknown"));
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
            "teiryod 2026.10.07-f78c2ce1a2b3\n\
             Teiryo daemon.\n\n\
             Usage: teiryod [OPTIONS]\n\n\
             Options:\n  \
               -h, --help     Print help\n  \
               -V, --version  Print version and build information"
        );
    }

    const PRINT: Flag = Flag {
        long: "--print-socket-path",
        value: None,
        help: "Print the socket path and exit",
    };

    const SSH: Flag = Flag {
        long: "--ssh",
        value: Some("HOST"),
        help: "Connect over ssh",
    };

    const BRIDGE: Flag = Flag {
        long: "bridge",
        value: None,
        help: "Relay stdio to the socket",
    };

    fn given(long: &'static str, value: Option<&str>) -> Given {
        Given {
            long,
            value: value.map(OsString::from),
        }
    }

    #[test]
    fn own_flags_parse_only_where_offered() {
        assert_eq!(
            parse_args_with(args(&["--print-socket-path"]), &[PRINT]),
            Invocation::Flags(vec![given("--print-socket-path", None)])
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

    /// A value flag takes the next argument verbatim, or the text after `=`,
    /// and repeated or mixed flags come back in the order given.
    #[test]
    fn value_flags_take_the_next_argument_or_an_inline_value() {
        assert_eq!(
            parse_args_with(args(&["--ssh", "user@box"]), &[SSH, PRINT]),
            Invocation::Flags(vec![given("--ssh", Some("user@box"))])
        );
        assert_eq!(
            parse_args_with(args(&["--ssh=box", "--print-socket-path"]), &[SSH, PRINT]),
            Invocation::Flags(vec![
                given("--ssh", Some("box")),
                given("--print-socket-path", None)
            ])
        );
        // The next argument is the value even when it looks like a flag.
        assert_eq!(
            parse_args_with(args(&["--ssh", "--print-socket-path"]), &[SSH, PRINT]),
            Invocation::Flags(vec![given("--ssh", Some("--print-socket-path"))])
        );
    }

    /// The falsifiers for the test above: a value flag with nothing after it
    /// is a usage error, `=` only splits a flag that takes a value, and help
    /// still wins.
    #[test]
    fn a_value_flag_without_its_value_is_an_error() {
        assert_eq!(
            parse_args_with(args(&["--ssh"]), &[SSH]),
            Invocation::MissingValue("--ssh")
        );
        assert_eq!(
            parse_args_with(args(&["--print-socket-path=x"]), &[PRINT]),
            Invocation::Unexpected("--print-socket-path=x".into())
        );
        assert_eq!(
            parse_args_with(args(&["--ssh", "box", "-h"]), &[SSH]),
            Invocation::Help
        );
        assert!(missing_value_error("teiryo", "--ssh").starts_with("teiryo: '--ssh' needs a value"));
    }

    #[test]
    fn help_lists_commands_and_value_placeholders() {
        let text = help_with("teiryod", "Teiryo daemon.", &SAMPLE, &[BRIDGE, SSH]);
        assert_eq!(
            text,
            "teiryod 2026.10.07-f78c2ce1a2b3\n\
             Teiryo daemon.\n\n\
             Usage: teiryod [OPTIONS] [COMMAND]\n\n\
             Commands:\n  \
               bridge            Relay stdio to the socket\n\n\
             Options:\n  \
               -h, --help        Print help\n  \
               -V, --version     Print version and build information\n  \
               \x20   --ssh <HOST>  Connect over ssh"
        );
    }

    #[test]
    fn help_aligns_own_flags_with_the_rest() {
        let text = help_with("teiryod", "Teiryo daemon.", &SAMPLE, &[PRINT]);
        assert_eq!(
            text,
            "teiryod 2026.10.07-f78c2ce1a2b3\n\
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
