//! `cargo xtask sim-runner`: builds the XCTest input runner for iOS
//! Simulators (`helpers/jidr-sim-runner`) with `xcodebuild build-for-testing`
//! and prints the `.xctestrun` file that `xcodebuild test-without-building`
//! runs.
//!
//! The runner is built on the user's Mac rather than shipped, so there is no
//! signed binary to notarize: simulator builds are signed ad hoc (`-`).

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use crate::workspace_root;

const USAGE: &str = "\
usage: cargo xtask sim-runner [--out <dir>]

Builds helpers/jidr-sim-runner for the iOS Simulator into <dir>
(default: <target>/jidr-sim-runner) and prints the .xctestrun path.";

const PROJECT: &str = "helpers/jidr-sim-runner/JidrSimRunner.xcodeproj";
const SCHEME: &str = "JidrSimRunner";

/// `Ok(None)` means `--help`; otherwise the chosen output dir, if any.
fn parse_args(args: &[String]) -> Result<Option<Option<PathBuf>>> {
    let mut out = None;
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--out" => out = Some(PathBuf::from(it.next().context("--out needs a directory")?)),
            "-h" | "--help" => return Ok(None),
            other => bail!("unknown argument `{other}`\n{USAGE}"),
        }
    }
    Ok(Some(out))
}

pub fn run(args: &[String]) -> Result<()> {
    let Some(out) = parse_args(args)? else {
        println!("{USAGE}");
        return Ok(());
    };
    let root = workspace_root();
    let out = match out {
        Some(dir) if dir.is_absolute() => dir,
        Some(dir) => std::env::current_dir()?.join(dir),
        None => crate::release::target_dir(&root).join("jidr-sim-runner"),
    };
    let xctestrun = build(&out)?;
    println!("{}", xctestrun.display());
    Ok(())
}

/// Builds the runner into the derived-data dir `out`; returns its `.xctestrun`.
pub fn build(out: &Path) -> Result<PathBuf> {
    if !cfg!(target_os = "macos") {
        bail!("the iOS Simulator runner builds only on macOS (needs Xcode)");
    }
    let derived = out.to_str().context("the output path is not UTF-8")?;
    crate::run(
        "xcodebuild",
        &[
            "build-for-testing",
            "-quiet",
            "-project",
            PROJECT,
            "-scheme",
            SCHEME,
            "-sdk",
            "iphonesimulator",
            "-destination",
            "generic/platform=iOS Simulator",
            "-derivedDataPath",
            derived,
            "CODE_SIGN_IDENTITY=-",
        ],
    )?;
    newest_xctestrun(&out.join("Build/Products"))
}

/// The most recently written `.xctestrun` in `products` (one per SDK version).
fn newest_xctestrun(products: &Path) -> Result<PathBuf> {
    let entries =
        std::fs::read_dir(products).with_context(|| format!("reading {}", products.display()))?;
    entries
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|ext| ext == "xctestrun"))
        .filter_map(|p| Some((std::fs::metadata(&p).ok()?.modified().ok()?, p)))
        .max()
        .map(|(_, p)| p)
        .with_context(|| format!("xcodebuild wrote no .xctestrun in {}", products.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_out_and_help() {
        let args = |a: &[&str]| a.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(parse_args(&[]).unwrap(), Some(None));
        assert_eq!(
            parse_args(&args(&["--out", "/tmp/r"])).unwrap(),
            Some(Some(PathBuf::from("/tmp/r")))
        );
        assert!(parse_args(&args(&["--out"])).is_err());
        assert!(parse_args(&args(&["--sdk"])).is_err());
        assert_eq!(parse_args(&args(&["--help"])).unwrap(), None);
    }

    #[test]
    fn finds_the_xctestrun() {
        let dir = std::env::temp_dir().join(format!("jidr-sim-runner-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        assert!(newest_xctestrun(&dir).is_err());
        let file = dir.join("JidrSimRunner_iphonesimulator27.1-arm64.xctestrun");
        std::fs::write(&file, "").unwrap();
        std::fs::write(dir.join("other.plist"), "").unwrap();
        assert_eq!(newest_xctestrun(&dir).unwrap(), file);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
