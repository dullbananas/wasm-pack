//! Functionality related to installing prebuilt binaries and/or running cargo install.

use self::krate::Krate;
use crate::child;
use crate::emoji;
use crate::install;
use crate::PBAR;
use anyhow::{anyhow, bail, Context, Result};
use binary_install::{Cache, Download};
use log::debug;
use log::{info, warn};
use std::env;
use std::ffi::OsStr;
use std::fs;
use std::path::Path;
use std::process::Command;
use which::which;

mod arch;
mod krate;
mod mode;
mod os;
mod tool;
pub use self::arch::Arch;
pub use self::mode::InstallMode;
pub use self::os::Os;
pub use self::tool::Tool;

/// Possible outcomes of attempting to find/install a tool
pub enum Status {
    /// Couldn't install tool because downloads are forbidden by user
    CannotInstall,
    /// The current platform doesn't support precompiled binaries for this tool
    PlatformNotSupported,
    /// We found the tool at the specified path
    Found(Download),
}

/// Handles possible installs status and returns the download or a error message
pub fn get_tool_path(status: &Status, tool: Tool) -> Result<&Download> {
    match status {
        Status::Found(download) => Ok(download),
        Status::CannotInstall => bail!("Not able to find or install a local {}.", tool.name()),
        install::Status::PlatformNotSupported => {
            bail!("{} does not currently support your platform.", tool.name())
        }
    }
}

/// Install a cargo CLI tool
///
/// Prefers an existing local install, if any exists. Then checks if there is a
/// global install on `$PATH` that fits the bill. Then attempts to download a
/// tarball from the GitHub releases page, if this target has prebuilt
/// binaries. Finally, falls back to `cargo install`.
pub fn download_prebuilt_or_cargo_install(
    tool: Tool,
    cache: &Cache,
    version: &str,
    install_permitted: bool,
) -> Result<Status> {
    // If the tool is installed globally and it has the right version, use
    // that. Assume that other tools are installed next to it.
    //
    // This situation can arise if the tool is already installed via
    // `cargo install`, for example.
    if let Ok(path) = which(tool.name()) {
        debug!("found global {} binary at: {}", tool.name(), path.display());
        if check_version(&tool, &path, version)? {
            let download = Download::at(path.parent().unwrap());
            return Ok(Status::Found(download));
        }
    }

    let msg = format!("{}Installing {}...", emoji::DOWN_ARROW, tool.name());
    PBAR.info(&msg);

    let dl = download_prebuilt(&tool, cache, version, install_permitted);
    match dl {
        Ok(dl) => return Ok(dl),
        Err(e) => {
            warn!(
                "could not download pre-built `{}`: {}. Falling back to `cargo install`.",
                tool.name(),
                e
            );
        }
    }

    cargo_install(tool, cache, version, install_permitted)
}

/// Check if the tool dependency is locally satisfied.
pub fn check_version(tool: &Tool, path: &Path, expected_version: &str) -> Result<bool> {
    let expected_version = if expected_version == "latest" {
        let krate = Krate::new(tool)?;
        krate.max_version
    } else {
        expected_version.to_string()
    };

    let v = get_cli_version(tool, path)?;
    info!(
        "Checking installed `{}` version == expected version: {} == {}",
        tool.name(),
        v,
        &expected_version
    );
    Ok(v == expected_version)
}

/// Fetches the version of a CLI tool
pub fn get_cli_version(tool: &Tool, path: &Path) -> Result<String> {
    let mut cmd = Command::new(path);
    cmd.arg("--version");
    let stdout = child::run_capture_stdout(cmd, tool)?;
    let version = stdout.split_whitespace().nth(1);
    match version {
        Some(v) => Ok(v.to_string()),
        None => bail!("Something went wrong! We couldn't determine your version of the wasm-bindgen CLI. We were supposed to set that up for you, so it's likely not your fault! You should file an issue: https://github.com/rustwasm/wasm-pack/issues/new?template=bug_report.md.")
    }
}

/// Downloads a precompiled copy of the tool, if available.
pub fn download_prebuilt(
    tool: &Tool,
    cache: &Cache,
    version: &str,
    install_permitted: bool,
) -> Result<Status> {
    let url = match prebuilt_url(tool, version) {
        Ok(url) => url,
        Err(e) => bail!(
            "no prebuilt {} binaries are available for this platform: {}",
            tool.name(),
            e,
        ),
    };
    match tool {
        Tool::WasmBindgen => {
            let binaries = &["wasm-bindgen", "wasm-bindgen-test-runner"];
            match cache.download(install_permitted, "wasm-bindgen", binaries, &url)? {
                Some(download) => Ok(Status::Found(download)),
                None => bail!("wasm-bindgen v{} is not installed!", version),
            }
        }
        Tool::CargoGenerate => {
            let binaries = &["cargo-generate"];
            match cache.download(install_permitted, "cargo-generate", binaries, &url)? {
                Some(download) => Ok(Status::Found(download)),
                None => bail!("cargo-generate v{} is not installed!", version),
            }
        }
        Tool::WasmOpt => {
            let binaries: &[&str] = match Os::get()? {
                Os::MacOS => &["bin/wasm-opt", "lib/libbinaryen.dylib"],
                Os::Linux => &["bin/wasm-opt"],
                Os::Windows => &["bin/wasm-opt.exe"],
            };
            match cache.download(install_permitted, "wasm-opt", binaries, &url)? {
                Some(download) => Ok(Status::Found(download)),
                // TODO(ag_dubs): why is this different? i forget...
                None => Ok(Status::CannotInstall),
            }
        }
    }
}

/// Returns the URL of a precompiled version of wasm-bindgen, if we have one
/// available for our host platform.
fn prebuilt_url(tool: &Tool, version: &str) -> Result<String> {
    let os = Os::get()?;
    let arch = Arch::get()?;
    prebuilt_url_for(tool, version, &arch, &os)
}

/// Get the download URL for some tool at some version, architecture and operating system
pub fn prebuilt_url_for(tool: &Tool, version: &str, arch: &Arch, os: &Os) -> Result<String> {
    let target = match (os, arch, tool) {
        (Os::Linux, Arch::X86_64, Tool::WasmOpt) => "x86_64-linux",
        (Os::Linux, Arch::X86_64, _) => "x86_64-unknown-linux-musl",
        (Os::MacOS, Arch::X86_64, Tool::WasmOpt) => "x86_64-macos",
        (Os::MacOS, Arch::X86_64, _) => "x86_64-apple-darwin",
        (Os::MacOS, Arch::AArch64, Tool::CargoGenerate) => "aarch64-apple-darwin",
        (Os::MacOS, Arch::AArch64, Tool::WasmOpt) => "arm64-macos",
        (Os::Windows, Arch::X86_64, Tool::WasmOpt) => "x86_64-windows",
        (Os::Windows, Arch::X86_64, _) => "x86_64-pc-windows-msvc",
        _ => bail!("Unrecognized target!"),
    };
    match tool {
        Tool::WasmBindgen => {
            Ok(format!(
                "https://github.com/rustwasm/wasm-bindgen/releases/download/{0}/wasm-bindgen-{0}-{1}.tar.gz",
                version,
                target
            ))
        },
        Tool::CargoGenerate => {
            Ok(format!(
                "https://github.com/cargo-generate/cargo-generate/releases/download/v{0}/cargo-generate-v{0}-{1}.tar.gz",
                "0.17.3",
                target
            ))
        },
        Tool::WasmOpt => {
            Ok(format!(
        "https://github.com/WebAssembly/binaryen/releases/download/{vers}/binaryen-{vers}-{target}.tar.gz",
        vers = "version_111",
        target = target,
            ))
        }
    }
}

/// Use `cargo install` to install the tool locally into the given
/// crate.
pub fn cargo_install(
    tool: Tool,
    cache: &Cache,
    version: &str,
    install_permitted: bool,
) -> Result<Status> {
    debug!(
        "Attempting to use a `cargo install`ed version of `{}={}`",
        tool.name(),
        version,
    );

    let dirname = format!("{}-cargo-install-{}", tool.name(), version);
    let destination = cache.join(dirname.as_ref());
    if destination.exists() {
        debug!(
            "`cargo install`ed `{}={}` already exists at {}",
            tool.name(),
            version,
            destination.display()
        );
        let download = Download::at(&destination);
        return Ok(Status::Found(download));
    }

    if !install_permitted {
        return Ok(Status::CannotInstall);
    }

    // Run `cargo install` to a temporary location to handle ctrl-c gracefully
    // and ensure we don't accidentally use stale files in the future
    let tmp = cache.join(format!(".{}", dirname).as_ref());
    drop(fs::remove_dir_all(&tmp));
    debug!(
        "cargo installing {} to tempdir: {}",
        tool.name(),
        tmp.display(),
    );

    let context = format!(
        "failed to create temp dir for `cargo install {}`",
        tool.name()
    );
    fs::create_dir_all(&tmp).context(context)?;

    let crate_name = match tool {
        Tool::WasmBindgen => "wasm-bindgen-cli",
        _ => tool.name(),
    };
    let mut cmd = Command::new("cargo");

    cmd.args(
        std::iter::empty::<&OsStr>()
            .chain([
                "install".as_ref(),
                "--force".as_ref(),
                crate_name.as_ref(),
                "--root".as_ref(),
                tmp.as_ref(),
            ])
            .chain(
                (version != "latest")
                    .then_some(["--version".as_ref(), version.as_ref()])
                    .into_iter()
                    .flatten(),
            ),
    );

    let context = format!("Installing {} with cargo", tool.name());
    child::run(cmd, "cargo install").context(context)?;

    // `cargo install` will put the installed binaries in `$root/bin/*`, but we
    // just want them in `$root/*` directly (which matches how the tarballs are
    // laid out, and where the rest of our code expects them to be). So we do a
    // little renaming here.
    let binaries: Result<&[&str]> = match tool {
        Tool::WasmBindgen => Ok(&["wasm-bindgen", "wasm-bindgen-test-runner"]),
        Tool::CargoGenerate => Ok(&["cargo-generate"]),
        Tool::WasmOpt => bail!("Cannot install wasm-opt with cargo."),
    };

    for b in binaries?.iter().cloned() {
        let from = tmp
            .join("bin")
            .join(b)
            .with_extension(env::consts::EXE_EXTENSION);
        let to = tmp.join(from.file_name().unwrap());
        fs::rename(&from, &to).with_context(|| {
            anyhow!(
                "failed to move {} to {} for `cargo install`ed `{}`",
                from.display(),
                to.display(),
                b
            )
        })?;
    }

    // Finally, move the `tmp` directory into our binary cache.
    fs::rename(&tmp, &destination)?;

    let download = Download::at(&destination);
    Ok(Status::Found(download))
}
