use shdeps::release_asset::{select, Target};

fn linux() -> Target {
    Target::new("linux", "x86_64", "gnu")
}

fn darwin() -> Target {
    Target::new("darwin", "aarch64", "gnu")
}

#[test]
fn matches_rust_triple_for_platform_and_arch() {
    let urls = [
        "https://example/tool-aarch64-unknown-linux-gnu.tar.gz",
        "https://example/tool-x86_64-unknown-linux-gnu.tar.gz",
        "https://example/tool-aarch64-apple-darwin.tar.gz",
    ];

    assert_eq!(
        select("tool", &urls, &linux()),
        Some("https://example/tool-x86_64-unknown-linux-gnu.tar.gz")
    );
}

#[test]
fn matches_go_plain_binary_conventions() {
    let urls = [
        "https://example/tool-linux-arm64",
        "https://example/tool-darwin-arm64",
        "https://example/tool-linux-amd64",
    ];

    assert_eq!(
        select("tool", &urls, &linux()),
        Some("https://example/tool-linux-amd64")
    );
}

#[test]
fn matching_is_case_insensitive() {
    let urls = [
        "https://example/tool_1.0_Darwin_arm64.tar.gz",
        "https://example/tool_1.0_Linux_x86_64.tar.gz",
    ];

    assert_eq!(
        select("tool", &urls, &linux()),
        Some("https://example/tool_1.0_Linux_x86_64.tar.gz")
    );
}

#[test]
fn skips_metadata_and_package_assets() {
    let urls = [
        "https://example/tool-linux-amd64.tar.gz.sha256",
        "https://example/tool-linux-amd64.deb",
        "https://example/tool-linux-amd64.tar.gz",
    ];

    assert_eq!(
        select("tool", &urls, &linux()),
        Some("https://example/tool-linux-amd64.tar.gz")
    );
}

#[test]
fn supports_os_aliases() {
    let urls = [
        "https://example/tool-macos-arm64.tar.gz",
        "https://example/tool-linux-x86_64.tar.gz",
    ];

    assert_eq!(
        select("tool", &urls, &darwin()),
        Some("https://example/tool-macos-arm64.tar.gz")
    );
    assert_eq!(
        select("tool", &urls, &linux()),
        Some("https://example/tool-linux-x86_64.tar.gz")
    );
}

#[test]
fn prefers_plain_binary_over_archives() {
    let urls = [
        "https://example/tool-linux-amd64.tar.gz",
        "https://example/tool-linux-amd64",
    ];

    assert_eq!(
        select("tool", &urls, &linux()),
        Some("https://example/tool-linux-amd64")
    );
}

#[test]
fn prefers_tar_archives_over_zip_archives() {
    let urls = [
        "https://example/tool-linux-amd64.zip",
        "https://example/tool-linux-amd64.tar.gz",
    ];

    assert_eq!(
        select("tool", &urls, &linux()),
        Some("https://example/tool-linux-amd64.tar.gz")
    );
}

#[test]
fn falls_back_to_zip_when_no_tar_archive_matches() {
    let urls = [
        "https://example/tool-linux-amd64.zip",
        "https://example/tool-linux-amd64.sha256",
    ];

    assert_eq!(
        select("tool", &urls, &linux()),
        Some("https://example/tool-linux-amd64.zip")
    );
}

#[test]
fn treats_single_file_compression_as_plain_assets() {
    let gz = [
        "https://example/tool-linux-amd64.tar.gz",
        "https://example/tool-linux-amd64.gz",
    ];
    let bz2 = ["https://example/tool_1.0_linux_amd64.bz2"];
    let zst = [
        "https://example/tool-x86_64-unknown-linux.tar.gz",
        "https://example/tool-x86_64-unknown-linux.zst",
    ];

    assert_eq!(
        select("tool", &gz, &linux()),
        Some("https://example/tool-linux-amd64.gz")
    );
    assert_eq!(
        select("tool", &bz2, &linux()),
        Some("https://example/tool_1.0_linux_amd64.bz2")
    );
    assert_eq!(
        select("tool", &zst, &linux()),
        Some("https://example/tool-x86_64-unknown-linux.zst")
    );
}

#[test]
fn treats_tar_zst_as_archive() {
    let urls = [
        "https://example/tool-x86_64-unknown-linux.zip",
        "https://example/tool-x86_64-unknown-linux.tar.zst",
    ];

    assert_eq!(
        select("tool", &urls, &linux()),
        Some("https://example/tool-x86_64-unknown-linux.tar.zst")
    );
}

#[test]
fn returns_none_when_no_asset_matches_platform() {
    let urls = [
        "https://example/tool-windows-amd64.exe",
        "https://example/tool-freebsd-amd64.tar.gz",
    ];

    assert_eq!(select("tool", &urls, &linux()), None);
}

#[test]
fn prefers_matching_linux_libc() {
    let urls = [
        "https://example/tool-x86_64-unknown-linux-musl.tar.gz",
        "https://example/tool-x86_64-unknown-linux-gnu.tar.gz",
    ];

    assert_eq!(
        select("tool", &urls, &linux()),
        Some("https://example/tool-x86_64-unknown-linux-gnu.tar.gz")
    );
}

#[test]
fn falls_back_to_available_linux_libc() {
    let urls = ["https://example/tool-x86_64-unknown-linux-musl.tar.gz"];

    assert_eq!(
        select("tool", &urls, &linux()),
        Some("https://example/tool-x86_64-unknown-linux-musl.tar.gz")
    );
}

#[test]
fn prefers_exact_command_name_for_multi_binary_releases() {
    let urls = [
        "https://example/codex-responses-api-proxy-x86_64-unknown-linux-gnu.tar.gz",
        "https://example/codex-x86_64-unknown-linux-gnu.tar.gz",
        "https://example/codex-command-runner-x86_64-unknown-linux-gnu.tar.gz",
    ];

    assert_eq!(
        select("codex", &urls, &linux()),
        Some("https://example/codex-x86_64-unknown-linux-gnu.tar.gz")
    );
}

#[test]
fn exact_command_name_accepts_common_separators_and_version_prefixes() {
    let underscore = [
        "https://example/tool_extra_linux_amd64.tar.gz",
        "https://example/tool_linux_amd64.tar.gz",
    ];
    let dot = [
        "https://example/tool-extra.v1.0.0.linux-amd64.tar.gz",
        "https://example/tool.v1.0.0.linux-amd64.tar.gz",
    ];
    let version = [
        "https://example/tool-extra-v1.0.0-linux-amd64.tar.gz",
        "https://example/tool-v1.0.0-linux-amd64.tar.gz",
    ];

    assert_eq!(
        select("tool", &underscore, &linux()),
        Some("https://example/tool_linux_amd64.tar.gz")
    );
    assert_eq!(
        select("tool", &dot, &linux()),
        Some("https://example/tool.v1.0.0.linux-amd64.tar.gz")
    );
    assert_eq!(
        select("tool", &version, &linux()),
        Some("https://example/tool-v1.0.0-linux-amd64.tar.gz")
    );
}

#[test]
fn falls_back_to_non_exact_when_no_exact_asset_exists() {
    let urls = ["https://example/tool-extended-linux-amd64.tar.gz"];

    assert_eq!(
        select("tool", &urls, &linux()),
        Some("https://example/tool-extended-linux-amd64.tar.gz")
    );
}

#[test]
fn bare_command_name_without_os_or_arch_is_not_enough() {
    let urls = [
        "https://example/tool",
        "https://example/tool-extra-linux-amd64",
    ];

    assert_eq!(
        select("tool", &urls, &linux()),
        Some("https://example/tool-extra-linux-amd64")
    );
}
