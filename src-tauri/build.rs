//! Stages a placeholder sidecar when the real one has not been built, then
//! runs `tauri-build`.
//!
//! `bundle.externalBin` names `binaries/rdirstat-runner`, and `tauri-build`
//! fails the whole build when `binaries/rdirstat-runner-<target>` is missing.
//! `pnpm tauri dev|build` stage the real CLI first (`scripts/build-sidecar.mjs`
//! runs in `beforeDevCommand` / `beforeBuildCommand`); a plain `cargo build`,
//! `cargo test` or `cargo clippy` does not, so this writes a script that
//! exits 127 with instructions. The app asks the sidecar for its version
//! before installing it on a drive, so a placeholder is refused there with
//! that message rather than copied.

use std::path::PathBuf;

fn main() {
    stage_placeholder_sidecar();
    tauri_build::build();
}

fn stage_placeholder_sidecar() {
    let Ok(target) = std::env::var("TARGET") else {
        return;
    };
    let Ok(manifest_dir) = std::env::var("CARGO_MANIFEST_DIR") else {
        return;
    };
    let path = PathBuf::from(manifest_dir)
        .join("binaries")
        .join(format!("rdirstat-runner-{target}"));
    if path.exists() {
        return;
    }
    let script = "#!/bin/sh\n\
                  echo 'rdirstat-runner is a placeholder: build the sidecar with `node scripts/build-sidecar.mjs`' >&2\n\
                  exit 127\n";
    let written = path
        .parent()
        .map_or(Ok(()), std::fs::create_dir_all)
        .and_then(|()| std::fs::write(&path, script))
        .and_then(|()| {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
        });
    if let Err(error) = written {
        println!(
            "cargo:warning=cannot stage a placeholder sidecar at {}: {error}",
            path.display()
        );
    }
}
