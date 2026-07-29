fn main() {
    emit_build_info();
    generate_windows_icon();

    tauri_build::build()
}

/// Embed a visible build identity (git short sha + build time) so a running
/// build can be traced back to the exact source it came from. Degrades
/// gracefully when git is unavailable (packaging, CI without a checkout).
fn emit_build_info() {
    use std::path::Path;

    println!("cargo:rerun-if-env-changed=SOURCE_DATE_EPOCH");

    // Re-run whenever the checkout moves (commit, checkout, reset) so the
    // embedded sha never goes stale on incremental dev builds. A commit updates
    // the current branch's ref file, not `HEAD`, so we must track that ref too;
    // refs may also be packed into `packed-refs`. `git rev-parse --git-path`
    // resolves each against the right location, including linked worktrees
    // (HEAD is per-worktree; branch refs / packed-refs live in the common dir).
    let mut watch = vec![git_path("HEAD"), git_path("packed-refs")];
    if let Some(branch_ref) = git_output(&["symbolic-ref", "-q", "HEAD"]) {
        // e.g. refs/heads/feat/build-info — the loose file bumped on commit.
        watch.push(git_path(&branch_ref));
    }
    for path in watch.into_iter().flatten() {
        if Path::new(&path).exists() {
            println!("cargo:rerun-if-changed={path}");
        }
    }

    let sha =
        git_output(&["rev-parse", "--short", "HEAD"]).unwrap_or_else(|| "unknown".to_string());
    println!("cargo:rustc-env=LUMEN_GIT_SHA={sha}");

    println!("cargo:rustc-env=LUMEN_BUILD_TIME={}", build_timestamp());
}

/// Resolve `name` (e.g. `HEAD`, `packed-refs`, `refs/heads/x`) to its on-disk
/// path via `git rev-parse --git-path`, relative to the crate root (the build
/// script's working dir), or `None` when git is unavailable.
fn git_path(name: &str) -> Option<String> {
    git_output(&["rev-parse", "--git-path", name])
}

/// Run `git <args>` and return trimmed non-empty stdout, or `None` if git is
/// missing or the command fails (e.g. not a git checkout).
fn git_output(args: &[&str]) -> Option<String> {
    let output = std::process::Command::new("git").args(args).output().ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8(output.stdout).ok()?.trim().to_string();
    (!text.is_empty()).then_some(text)
}

/// UTC build timestamp. Honors `SOURCE_DATE_EPOCH` for reproducible builds,
/// otherwise uses wall-clock time at compile.
fn build_timestamp() -> String {
    let dt = std::env::var("SOURCE_DATE_EPOCH")
        .ok()
        .and_then(|v| v.parse::<i64>().ok())
        .and_then(|secs| chrono::DateTime::from_timestamp(secs, 0))
        .unwrap_or_else(chrono::Utc::now);
    dt.format("%Y-%m-%d %H:%M UTC").to_string()
}

fn generate_windows_icon() {
    use std::fs::File;
    use std::path::Path;

    let output = Path::new("icons/icon.ico");
    if output.exists() {
        return;
    }

    let mut icon = ico::IconDir::new(ico::ResourceType::Icon);
    for source in ["icons/32x32.png", "icons/128x128.png"] {
        let image = ico::IconImage::read_png(File::open(source).unwrap_or_else(|error| {
            panic!("failed to open Windows icon source {source}: {error}")
        }))
        .unwrap_or_else(|error| panic!("failed to decode Windows icon source {source}: {error}"));
        icon.add_entry(
            ico::IconDirEntry::encode(&image)
                .unwrap_or_else(|error| panic!("failed to encode {source} into ICO: {error}")),
        );
    }

    icon.write(
        File::create(output)
            .unwrap_or_else(|error| panic!("failed to create {}: {error}", output.display())),
    )
    .unwrap_or_else(|error| panic!("failed to write {}: {error}", output.display()));
}
