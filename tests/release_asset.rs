use shdeps::release_asset::{Target, select};

fn linux() -> Target {
    Target::new("linux", "x86_64", "gnu")
}

fn darwin() -> Target {
    Target::new("darwin", "aarch64", "gnu")
}

fn linux_musl() -> Target {
    Target::new("linux", "x86_64", "musl")
}

#[test]
fn matches_rust_triple_for_platform_and_arch() {
    let urls = [
        "https://github.com/owner/tool/releases/download/v1/tool-aarch64-unknown-linux-gnu.tar.gz",
        "https://github.com/owner/tool/releases/download/v1/tool-x86_64-unknown-linux-gnu.tar.gz",
        "https://github.com/owner/tool/releases/download/v1/tool-aarch64-apple-darwin.tar.gz",
    ];

    assert_eq!(
        select("tool", &urls, &linux()),
        Some(
            "https://github.com/owner/tool/releases/download/v1/tool-x86_64-unknown-linux-gnu.tar.gz"
        )
    );
}

#[test]
fn matches_go_plain_binary_conventions() {
    let urls = [
        "https://github.com/owner/tool/releases/download/v1/tool-linux-arm64",
        "https://github.com/owner/tool/releases/download/v1/tool-darwin-arm64",
        "https://github.com/owner/tool/releases/download/v1/tool-linux-amd64",
    ];

    assert_eq!(
        select("tool", &urls, &linux()),
        Some("https://github.com/owner/tool/releases/download/v1/tool-linux-amd64")
    );
}

#[test]
fn matching_is_case_insensitive() {
    let urls = [
        "https://github.com/owner/tool/releases/download/v1/tool_1.0_Darwin_arm64.tar.gz",
        "https://github.com/owner/tool/releases/download/v1/tool_1.0_Linux_x86_64.tar.gz",
    ];

    assert_eq!(
        select("tool", &urls, &linux()),
        Some("https://github.com/owner/tool/releases/download/v1/tool_1.0_Linux_x86_64.tar.gz")
    );
}

#[test]
fn skips_metadata_and_package_assets() {
    let urls = [
        "https://github.com/owner/tool/releases/download/v1/tool-linux-amd64.tar.gz.sha256",
        "https://github.com/owner/tool/releases/download/v1/tool-linux-amd64.sha256sum",
        "https://github.com/owner/tool/releases/download/v1/tool-linux-amd64-checksums",
        "https://github.com/owner/tool/releases/download/v1/tool-linux-amd64.intoto.jsonl",
        "https://github.com/owner/tool/releases/download/v1/tool-linux-amd64.deb",
        "https://github.com/owner/tool/releases/download/v1/tool-linux-amd64.tar.gz",
    ];

    assert_eq!(
        select("tool", &urls, &linux()),
        Some("https://github.com/owner/tool/releases/download/v1/tool-linux-amd64.tar.gz")
    );
}

#[test]
fn skips_unsupported_archives_instead_of_treating_them_as_plain_binaries() {
    let urls = [
        "https://github.com/owner/tool/releases/download/v1/tool-linux-amd64.7z",
        "https://github.com/owner/tool/releases/download/v1/tool-linux-amd64.tar.lz4",
        "https://github.com/owner/tool/releases/download/v1/tool-linux-amd64.tar.gz",
    ];

    assert_eq!(
        select("tool", &urls, &linux()),
        Some("https://github.com/owner/tool/releases/download/v1/tool-linux-amd64.tar.gz")
    );
}

#[test]
fn supports_os_aliases() {
    let urls = [
        "https://github.com/owner/tool/releases/download/v1/tool-macos-arm64.tar.gz",
        "https://github.com/owner/tool/releases/download/v1/tool-linux-x86_64.tar.gz",
    ];

    assert_eq!(
        select("tool", &urls, &darwin()),
        Some("https://github.com/owner/tool/releases/download/v1/tool-macos-arm64.tar.gz")
    );
    assert_eq!(
        select("tool", &urls, &linux()),
        Some("https://github.com/owner/tool/releases/download/v1/tool-linux-x86_64.tar.gz")
    );
}

#[test]
fn supports_more_archive_and_arch_aliases() {
    let urls = [
        "https://github.com/owner/tool/releases/download/v1/tool-linux-x86-64.txz",
        "https://github.com/owner/tool/releases/download/v1/tool-linux-amd64.tbz2",
    ];

    assert_eq!(
        select("tool", &urls, &linux()),
        Some("https://github.com/owner/tool/releases/download/v1/tool-linux-x86-64.txz")
    );
}

#[test]
fn supports_macos_universal_assets() {
    let urls = [
        "https://github.com/owner/tool/releases/download/v1/tool-linux-amd64.tar.gz",
        "https://github.com/owner/tool/releases/download/v1/tool-mac-universal.zip",
    ];

    assert_eq!(
        select("tool", &urls, &darwin()),
        Some("https://github.com/owner/tool/releases/download/v1/tool-mac-universal.zip")
    );
}

#[test]
fn short_macos_alias_requires_token_boundary() {
    let urls = ["https://github.com/owner/emacs/releases/download/v1/emacs-linux-arm64.tar.gz"];

    assert_eq!(select("emacs", &urls, &darwin()), None);
}

#[test]
fn prefers_plain_binary_over_archives() {
    let urls = [
        "https://github.com/owner/tool/releases/download/v1/tool-linux-amd64.tar.gz",
        "https://github.com/owner/tool/releases/download/v1/tool-linux-amd64",
    ];

    assert_eq!(
        select("tool", &urls, &linux()),
        Some("https://github.com/owner/tool/releases/download/v1/tool-linux-amd64")
    );
}

#[test]
fn prefers_tar_archives_over_zip_archives() {
    let urls = [
        "https://github.com/owner/tool/releases/download/v1/tool-linux-amd64.zip",
        "https://github.com/owner/tool/releases/download/v1/tool-linux-amd64.tar.gz",
    ];

    assert_eq!(
        select("tool", &urls, &linux()),
        Some("https://github.com/owner/tool/releases/download/v1/tool-linux-amd64.tar.gz")
    );
}

#[test]
fn falls_back_to_zip_when_no_tar_archive_matches() {
    let urls = [
        "https://github.com/owner/tool/releases/download/v1/tool-linux-amd64.zip",
        "https://github.com/owner/tool/releases/download/v1/tool-linux-amd64.sha256",
    ];

    assert_eq!(
        select("tool", &urls, &linux()),
        Some("https://github.com/owner/tool/releases/download/v1/tool-linux-amd64.zip")
    );
}

#[test]
fn treats_single_file_compression_as_plain_assets() {
    let gz = [
        "https://github.com/owner/tool/releases/download/v1/tool-linux-amd64.tar.gz",
        "https://github.com/owner/tool/releases/download/v1/tool-linux-amd64.gz",
    ];
    let bz2 = ["https://github.com/owner/tool/releases/download/v1/tool_1.0_linux_amd64.bz2"];
    let zst = [
        "https://github.com/owner/tool/releases/download/v1/tool-x86_64-unknown-linux.tar.gz",
        "https://github.com/owner/tool/releases/download/v1/tool-x86_64-unknown-linux.zst",
    ];

    assert_eq!(
        select("tool", &gz, &linux()),
        Some("https://github.com/owner/tool/releases/download/v1/tool-linux-amd64.gz")
    );
    assert_eq!(
        select("tool", &bz2, &linux()),
        Some("https://github.com/owner/tool/releases/download/v1/tool_1.0_linux_amd64.bz2")
    );
    assert_eq!(
        select("tool", &zst, &linux()),
        Some("https://github.com/owner/tool/releases/download/v1/tool-x86_64-unknown-linux.zst")
    );
}

#[test]
fn treats_tar_zst_as_archive() {
    let urls = [
        "https://github.com/owner/tool/releases/download/v1/tool-x86_64-unknown-linux.zip",
        "https://github.com/owner/tool/releases/download/v1/tool-x86_64-unknown-linux.tar.zst",
    ];

    assert_eq!(
        select("tool", &urls, &linux()),
        Some(
            "https://github.com/owner/tool/releases/download/v1/tool-x86_64-unknown-linux.tar.zst"
        )
    );
}

#[test]
fn returns_none_when_no_asset_matches_platform() {
    let urls = [
        "https://github.com/owner/tool/releases/download/v1/tool-windows-amd64.exe",
        "https://github.com/owner/tool/releases/download/v1/tool-freebsd-amd64.tar.gz",
    ];

    assert_eq!(select("tool", &urls, &linux()), None);
}

#[test]
fn prefers_matching_linux_libc() {
    let urls = [
        "https://github.com/owner/tool/releases/download/v1/tool-x86_64-unknown-linux-musl.tar.gz",
        "https://github.com/owner/tool/releases/download/v1/tool-x86_64-unknown-linux-gnu.tar.gz",
    ];

    assert_eq!(
        select("tool", &urls, &linux()),
        Some(
            "https://github.com/owner/tool/releases/download/v1/tool-x86_64-unknown-linux-gnu.tar.gz"
        )
    );
}

#[test]
fn falls_back_to_available_linux_libc() {
    let urls = [
        "https://github.com/owner/tool/releases/download/v1/tool-x86_64-unknown-linux-musl.tar.gz",
    ];

    assert_eq!(
        select("tool", &urls, &linux()),
        Some(
            "https://github.com/owner/tool/releases/download/v1/tool-x86_64-unknown-linux-musl.tar.gz"
        )
    );
}

#[test]
fn prefers_generic_linux_asset_over_wrong_libc() {
    let urls = [
        "https://github.com/owner/tool/releases/download/v1/tool-linux-amd64-musl.tar.gz",
        "https://github.com/owner/tool/releases/download/v1/tool-linux-amd64.tar.gz",
    ];

    assert_eq!(
        select("tool", &urls, &linux()),
        Some("https://github.com/owner/tool/releases/download/v1/tool-linux-amd64.tar.gz")
    );
}

#[test]
fn treats_glibc_as_gnu_libc() {
    let urls = [
        "https://github.com/owner/tool/releases/download/v1/tool-linux-amd64-musl.tar.gz",
        "https://github.com/owner/tool/releases/download/v1/tool-linux-amd64-glibc.tar.gz",
    ];

    assert_eq!(
        select("tool", &urls, &linux()),
        Some("https://github.com/owner/tool/releases/download/v1/tool-linux-amd64-glibc.tar.gz")
    );
}

#[test]
fn prefers_musl_asset_on_musl_hosts() {
    let urls = [
        "https://github.com/owner/tool/releases/download/v1/tool-linux-amd64.tar.gz",
        "https://github.com/owner/tool/releases/download/v1/tool-linux-amd64-musl.tar.gz",
    ];

    assert_eq!(
        select("tool", &urls, &linux_musl()),
        Some("https://github.com/owner/tool/releases/download/v1/tool-linux-amd64-musl.tar.gz")
    );
}

#[test]
fn platform_tokens_in_repo_or_tag_path_do_not_make_filename_match() {
    let urls = [
        "https://github.com/linux-owner/tool/releases/download/x86_64/tool.tar.gz",
        "https://github.com/owner/tool/releases/download/v1/tool-linux-amd64.tar.gz",
    ];

    assert_eq!(
        select("tool", &urls, &linux()),
        Some("https://github.com/owner/tool/releases/download/v1/tool-linux-amd64.tar.gz")
    );
}

#[test]
fn prefers_exact_command_name_for_multi_binary_releases() {
    let urls = [
        "https://github.com/owner/tool/releases/download/v1/codex-responses-api-proxy-x86_64-unknown-linux-gnu.tar.gz",
        "https://github.com/owner/tool/releases/download/v1/codex-x86_64-unknown-linux-gnu.tar.gz",
        "https://github.com/owner/tool/releases/download/v1/codex-command-runner-x86_64-unknown-linux-gnu.tar.gz",
    ];

    assert_eq!(
        select("codex", &urls, &linux()),
        Some(
            "https://github.com/owner/tool/releases/download/v1/codex-x86_64-unknown-linux-gnu.tar.gz"
        )
    );
}

#[test]
fn exact_command_name_accepts_common_separators_and_version_prefixes() {
    let underscore = [
        "https://github.com/owner/tool/releases/download/v1/tool_extra_linux_amd64.tar.gz",
        "https://github.com/owner/tool/releases/download/v1/tool_linux_amd64.tar.gz",
    ];
    let dot = [
        "https://github.com/owner/tool/releases/download/v1/tool-extra.v1.0.0.linux-amd64.tar.gz",
        "https://github.com/owner/tool/releases/download/v1/tool.v1.0.0.linux-amd64.tar.gz",
    ];
    let version = [
        "https://github.com/owner/tool/releases/download/v1/tool-extra-v1.0.0-linux-amd64.tar.gz",
        "https://github.com/owner/tool/releases/download/v1/tool-v1.0.0-linux-amd64.tar.gz",
    ];

    assert_eq!(
        select("tool", &underscore, &linux()),
        Some("https://github.com/owner/tool/releases/download/v1/tool_linux_amd64.tar.gz")
    );
    assert_eq!(
        select("tool", &dot, &linux()),
        Some("https://github.com/owner/tool/releases/download/v1/tool.v1.0.0.linux-amd64.tar.gz")
    );
    assert_eq!(
        select("tool", &version, &linux()),
        Some("https://github.com/owner/tool/releases/download/v1/tool-v1.0.0-linux-amd64.tar.gz")
    );
}

#[test]
fn falls_back_to_non_exact_when_no_exact_asset_exists() {
    let urls =
        ["https://github.com/owner/tool/releases/download/v1/tool-extended-linux-amd64.tar.gz"];

    assert_eq!(
        select("tool", &urls, &linux()),
        Some("https://github.com/owner/tool/releases/download/v1/tool-extended-linux-amd64.tar.gz")
    );
}

#[test]
fn bare_command_name_without_os_or_arch_is_not_enough() {
    let urls = [
        "https://github.com/owner/tool/releases/download/v1/tool",
        "https://github.com/owner/tool/releases/download/v1/tool-extra-linux-amd64",
    ];

    assert_eq!(
        select("tool", &urls, &linux()),
        Some("https://github.com/owner/tool/releases/download/v1/tool-extra-linux-amd64")
    );
}
