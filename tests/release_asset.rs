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

fn linux_i686() -> Target {
    Target::new("linux", "i686", "gnu")
}

fn linux_armv7() -> Target {
    Target::new("linux", "armv7l", "gnu")
}

fn linux_loongarch64() -> Target {
    Target::new("linux", "loongarch64", "gnu")
}

fn linux_riscv64() -> Target {
    Target::new("linux", "riscv64", "gnu")
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
        "https://github.com/owner/tool/releases/download/v1/tool-linux-amd64.bsdiff",
        "https://github.com/owner/tool/releases/download/v1/tool-linux-amd64.patch",
        "https://github.com/owner/tool/releases/download/v1/tool-linux-amd64.pkg.tar.zst",
        "https://github.com/owner/tool/releases/download/v1/tool-linux-amd64.whl",
        "https://github.com/owner/tool/releases/download/v1/tool-linux-amd64.sha",
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
fn supports_linux64_x86_64_alias() {
    let urls = [
        "https://github.com/owner/tool/releases/download/v1/tool-linux-arm64.tar.gz",
        "https://github.com/owner/tool/releases/download/v1/tool-linux64.tar.gz",
    ];

    assert_eq!(
        select("tool", &urls, &linux()),
        Some("https://github.com/owner/tool/releases/download/v1/tool-linux64.tar.gz")
    );
}

#[test]
fn supports_linux_distribution_os_aliases() {
    let urls = [
        "https://github.com/owner/tool/releases/download/v1/tool-darwin-arm64.tar.gz",
        "https://github.com/owner/tool/releases/download/v1/tool-ubuntu-amd64.tar.gz",
    ];

    assert_eq!(
        select("tool", &urls, &linux()),
        Some("https://github.com/owner/tool/releases/download/v1/tool-ubuntu-amd64.tar.gz")
    );
}

#[test]
fn supports_pep_platform_tag_linux_aliases() {
    let manylinux = [
        "https://github.com/owner/tool/releases/download/v1/tool-macos-arm64.tar.gz",
        "https://github.com/owner/tool/releases/download/v1/tool-manylinux2014_x86_64.tar.gz",
    ];
    let musllinux = [
        "https://github.com/owner/tool/releases/download/v1/tool-linux-aarch64.tar.gz",
        "https://github.com/owner/tool/releases/download/v1/tool-musllinux_1_2_x86_64.tar.gz",
    ];

    assert_eq!(
        select("tool", &manylinux, &linux()),
        Some("https://github.com/owner/tool/releases/download/v1/tool-manylinux2014_x86_64.tar.gz")
    );
    assert_eq!(
        select("tool", &musllinux, &linux()),
        Some("https://github.com/owner/tool/releases/download/v1/tool-musllinux_1_2_x86_64.tar.gz")
    );
}

#[test]
fn numbered_platform_aliases_require_boundaries() {
    let urls =
        ["https://github.com/owner/tool/releases/download/v1/tool-manylinux2014bad_x86_64.tar.gz"];

    assert_eq!(select("tool", &urls, &linux()), None);
}

#[test]
fn supports_additional_arch_aliases() {
    let x86 = [
        "https://github.com/owner/tool/releases/download/v1/tool-linux-amd64.tar.gz",
        "https://github.com/owner/tool/releases/download/v1/tool-linux-386.tar.gz",
    ];
    let arm = [
        "https://github.com/owner/tool/releases/download/v1/tool-linux-arm64.tar.gz",
        "https://github.com/owner/tool/releases/download/v1/tool-linux-armv7.tar.gz",
    ];
    let loong = ["https://github.com/owner/tool/releases/download/v1/tool-linux-loong64.tar.gz"];
    let riscv = ["https://github.com/owner/tool/releases/download/v1/tool-linux-riscv64gc.tar.gz"];

    assert_eq!(
        select("tool", &x86, &linux_i686()),
        Some("https://github.com/owner/tool/releases/download/v1/tool-linux-386.tar.gz")
    );
    assert_eq!(
        select("tool", &arm, &linux_armv7()),
        Some("https://github.com/owner/tool/releases/download/v1/tool-linux-armv7.tar.gz")
    );
    assert_eq!(
        select("tool", &loong, &linux_loongarch64()),
        Some("https://github.com/owner/tool/releases/download/v1/tool-linux-loong64.tar.gz")
    );
    assert_eq!(
        select("tool", &riscv, &linux_riscv64()),
        Some("https://github.com/owner/tool/releases/download/v1/tool-linux-riscv64gc.tar.gz")
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
fn prefers_normal_asset_over_baseline_and_profile_variants() {
    let urls = [
        "https://github.com/owner/tool/releases/download/v1/tool-linux-x64-baseline-profile.zip",
        "https://github.com/owner/tool/releases/download/v1/tool-linux-x64-profile.zip",
        "https://github.com/owner/tool/releases/download/v1/tool-linux-x64-baseline.zip",
        "https://github.com/owner/tool/releases/download/v1/tool-linux-x64.zip",
    ];

    assert_eq!(
        select("tool", &urls, &linux()),
        Some("https://github.com/owner/tool/releases/download/v1/tool-linux-x64.zip")
    );
}

#[test]
fn keeps_baseline_asset_as_fallback_when_normal_variant_is_missing() {
    let urls = [
        "https://github.com/owner/tool/releases/download/v1/tool-linux-x64-profile.zip",
        "https://github.com/owner/tool/releases/download/v1/tool-linux-x64-baseline.zip",
    ];

    assert_eq!(
        select("tool", &urls, &linux()),
        Some("https://github.com/owner/tool/releases/download/v1/tool-linux-x64-baseline.zip")
    );
}

#[test]
fn libc_match_beats_build_variant_penalty() {
    let urls = [
        "https://github.com/owner/tool/releases/download/v1/tool-linux-x64.zip",
        "https://github.com/owner/tool/releases/download/v1/tool-linux-x64-musl-baseline.zip",
    ];

    assert_eq!(
        select("tool", &urls, &linux_musl()),
        Some("https://github.com/owner/tool/releases/download/v1/tool-linux-x64-musl-baseline.zip")
    );
}

#[test]
fn prefers_shorter_same_score_asset_over_arrival_order() {
    let urls = [
        "https://github.com/owner/tool/releases/download/v1/tool-linux-amd64-extra.tar.gz",
        "https://github.com/owner/tool/releases/download/v1/tool-linux-amd64.tar.gz",
    ];

    assert_eq!(
        select("tool", &urls, &linux()),
        Some("https://github.com/owner/tool/releases/download/v1/tool-linux-amd64.tar.gz")
    );
}

#[test]
fn penalizes_debug_build_variants() {
    let urls = [
        "https://github.com/owner/tool/releases/download/v1/tool-linux-amd64-debug.tar.gz",
        "https://github.com/owner/tool/releases/download/v1/tool-linux-amd64.tar.gz",
    ];

    assert_eq!(
        select("tool", &urls, &linux()),
        Some("https://github.com/owner/tool/releases/download/v1/tool-linux-amd64.tar.gz")
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
