//! Installed-tool version extraction.
//!
//! The process layer will own actually running `--version` and `-V` with a
//! timeout. This module only classifies captured output so the compatibility
//! rules are deterministic and cheap to test. That split matters because
//! version probing happens on warm `list`/`check` paths where surprising hangs
//! or expensive retries would make `shdeps` feel heavy.

/// Extracts the first compatible version from captured probe output.
///
/// `probes` must be ordered the same way the Bash reference probes commands:
/// `--version` first, then `-V`. Dotted versions are preferred across probes
/// before falling back to an integer-only token near the command name or the
/// word `version`.
#[must_use]
pub fn extract(probes: &[&str], command: &str) -> Option<String> {
    if command.is_empty() {
        return None;
    }

    let mut fallback_output = String::new();
    for output in probes {
        if failed_to_load(output) {
            continue;
        }
        if let Some(version) = first_dotted_version(output) {
            return Some(version);
        }
        fallback_output.push_str(output);
        fallback_output.push('\n');
    }

    first_integer_version_near_name(&fallback_output, command_name(command))
}

/// Returns whether probe output is a dynamic-loader failure, not tool output.
///
/// These messages contain version-looking tokens such as `GLIBC_2.39`, but they
/// describe the host ABI mismatch. Treating them as the tool version can make
/// reinstall loops look successful while the binary remains unrunnable.
#[must_use]
pub fn failed_to_load(output: &str) -> bool {
    (output.contains("GLIBC_") && output.contains("not found"))
        || (output.contains("GLIBCXX_") && output.contains("not found"))
        || (output.contains("CXXABI_") && output.contains("not found"))
        || output.contains("error while loading shared libraries")
}

fn first_dotted_version(output: &str) -> Option<String> {
    let bytes = output.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        if !bytes[index].is_ascii_digit() {
            index += 1;
            continue;
        }

        let start = index;
        while bytes.get(index).is_some_and(u8::is_ascii_digit) {
            index += 1;
        }
        if bytes.get(index) != Some(&b'.') || !bytes.get(index + 1).is_some_and(u8::is_ascii_digit)
        {
            index = start + 1;
            continue;
        }

        index += 2;
        while bytes
            .get(index)
            .is_some_and(|byte| byte.is_ascii_digit() || *byte == b'.' || byte.is_ascii_lowercase())
        {
            index += 1;
        }
        return Some(output[start..index].to_owned());
    }

    None
}

fn first_integer_version_near_name(output: &str, command_name: &str) -> Option<String> {
    let command_name = command_name.to_lowercase();
    for line in output.lines() {
        let lower = line.to_lowercase();
        if !lower.contains("version") && !lower.contains(&command_name) {
            continue;
        }
        if let Some(version) = first_integer_token(line) {
            return Some(version);
        }
    }

    None
}

fn first_integer_token(line: &str) -> Option<String> {
    let bytes = line.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        if !bytes[index].is_ascii_digit() {
            index += 1;
            continue;
        }

        let start = index;
        while bytes.get(index).is_some_and(u8::is_ascii_digit) {
            index += 1;
        }
        if bytes.get(index).is_some_and(u8::is_ascii_lowercase) {
            index += 1;
        }
        return Some(line[start..index].to_owned());
    }

    None
}

fn command_name(command: &str) -> &str {
    command.rsplit('/').next().unwrap_or(command)
}

#[cfg(test)]
mod tests {
    use super::{extract, failed_to_load};

    #[test]
    fn extracts_dotted_version_from_first_probe() {
        assert_eq!(
            extract(&["fake-tool version 3.14.159"], "fake-tool").as_deref(),
            Some("3.14.159")
        );
    }

    #[test]
    fn extracts_version_from_later_lines() {
        assert_eq!(
            extract(
                &["line2-tool - A modern replacement for foo\nv0.23.4 [+git]"],
                "line2-tool",
            )
            .as_deref(),
            Some("0.23.4")
        );
    }

    #[test]
    fn falls_back_to_second_probe_for_dotted_version() {
        assert_eq!(
            extract(&["unknown option", "dashV-tool 3.6a"], "dashV-tool").as_deref(),
            Some("3.6a")
        );
    }

    #[test]
    fn captures_versions_from_stderr_style_output() {
        assert_eq!(
            extract(
                &["illegal option", "OpenFoo_10.2p1, LibBar 3.3.6"],
                "stderr-tool",
            )
            .as_deref(),
            Some("10.2p1")
        );
    }

    #[test]
    fn accepts_nonzero_exit_output_when_text_is_valid() {
        assert_eq!(
            extract(&["ExitFail 6.00 of 20 April 2009"], "exitfail-tool").as_deref(),
            Some("6.00")
        );
    }

    #[test]
    fn ignores_dynamic_loader_versions() {
        let output = "loader-error-tool: /lib64/libm.so.6: version `GLIBC_2.35' not found\n\
                      loader-error-tool: /lib64/libc.so.6: version `GLIBC_2.39' not found";

        assert!(failed_to_load(output));
        assert_eq!(extract(&[output], "loader-error-tool"), None);
    }

    #[test]
    fn falls_back_to_integer_only_version_near_command_name() {
        assert_eq!(
            extract(
                &["intver-tool 668 (POSIX regular expressions)"],
                "intver-tool",
            )
            .as_deref(),
            Some("668")
        );
    }

    #[test]
    fn version_noise_does_not_shadow_later_dotted_probe() {
        assert_eq!(
            extract(
                &[
                    "usage: noisy-tool [-46AaCf] destination",
                    "noisy-tool 10.2p1",
                ],
                "noisy-tool",
            )
            .as_deref(),
            Some("10.2p1")
        );
    }

    #[test]
    fn returns_none_when_no_version_is_available() {
        assert_eq!(extract(&["no version here"], "nover-tool"), None);
        assert_eq!(extract(&["fake-tool version 1.2.3"], ""), None);
    }
}
