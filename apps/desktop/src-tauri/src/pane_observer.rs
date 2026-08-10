//! Read-only adapters for terminal multiplexers with stable pane identities.
//!
//! Discovery is deliberately conservative: a pane is accepted only when its
//! client/shell process belongs to the frontmost outer terminal captured at
//! dictation start. Providers never enumerate pane text during discovery.

use lumen_platform_macos::FrontmostTarget;
use std::collections::{HashMap, HashSet};
use std::ffi::OsString;
use std::fmt;
use std::fs;
use std::io::Read;
#[cfg(unix)]
use std::os::unix::fs::FileTypeExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

const COMMAND_TIMEOUT: Duration = Duration::from_millis(900);
const PROCESS_LIST_PROGRAM: &str = "/bin/ps";
const OSASCRIPT_PROGRAM: &str = "/usr/bin/osascript";
const FRONTMOST_PROCESS_SCRIPT: &str = r#"
tell application "System Events"
  set frontProcess to first application process whose frontmost is true
  return unix id of frontProcess as text
end tell
"#;

#[derive(Debug, Clone)]
pub(crate) struct PaneSnapshot {
    pub text: String,
}

trait PaneHandle: Send + Sync {
    fn observer_id(&self) -> &'static str;
    fn fingerprint_material(&self) -> String;
    fn snapshot(&self) -> Result<PaneSnapshot, String>;
}

/// A provider-owned, stable pane identity captured at dictation start.
#[derive(Clone)]
pub(crate) struct LockedPane {
    inner: Arc<dyn PaneHandle>,
}

impl fmt::Debug for LockedPane {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("LockedPane")
            .field("observer_id", &self.observer_id())
            .finish_non_exhaustive()
    }
}

impl LockedPane {
    pub fn observer_id(&self) -> &'static str {
        self.inner.observer_id()
    }

    pub fn fingerprint_material(&self) -> String {
        self.inner.fingerprint_material()
    }

    pub fn snapshot(&self) -> Result<PaneSnapshot, String> {
        self.inner.snapshot()
    }
}

trait CommandRunner: Send + Sync {
    fn run(&self, program: &Path, arguments: &[OsString]) -> Result<CommandOutput, String>;

    fn cancelled(&self) -> bool {
        false
    }
}

#[derive(Debug, Clone)]
struct OuterTerminalGuard {
    process_id: u32,
    surface: Option<OuterTerminalSurface>,
    terminal_tty: Option<String>,
}

#[derive(Debug, Clone)]
enum OuterTerminalSurface {
    Ghostty { terminal_id: String },
}

impl OuterTerminalGuard {
    fn capture(
        target: &FrontmostTarget,
        process_tree: &ProcessTree,
        runner: &dyn CommandRunner,
    ) -> Result<OuterTerminalGuard, String> {
        let process_id = target
            .process_id
            .ok_or_else(|| "outer_process_unavailable".to_owned())?;
        let surface = if is_ghostty(target) {
            let identity = read_ghostty_focused_terminal(runner)?;
            if identity.frontmost_process_id != process_id {
                return Err("outer_terminal_changed".to_owned());
            }
            Some(OuterTerminalSurface::Ghostty {
                terminal_id: identity.terminal_id,
            })
        } else {
            if read_frontmost_process_id(runner)? != process_id {
                return Err("outer_terminal_changed".to_owned());
            }
            None
        };
        let terminal_tty = if surface.is_none() {
            Some(
                process_tree
                    .single_terminal_tty(process_id)
                    .ok_or_else(|| "outer_terminal_tty_ambiguous".to_owned())?,
            )
        } else {
            None
        };
        Ok(Self {
            process_id,
            surface,
            terminal_tty,
        })
    }

    fn verify(&self, runner: &dyn CommandRunner) -> Result<(), String> {
        match self.surface.as_ref() {
            Some(OuterTerminalSurface::Ghostty { terminal_id, .. }) => {
                let current = read_ghostty_focused_terminal(runner)?;
                if current.frontmost_process_id != self.process_id {
                    return Err("outer_terminal_changed".to_owned());
                }
                if current.terminal_id != *terminal_id {
                    return Err("outer_terminal_surface_changed".to_owned());
                }
                Ok(())
            }
            None if read_frontmost_process_id(runner)? == self.process_id => Ok(()),
            None => Err("outer_terminal_changed".to_owned()),
        }
    }

    fn accepts_provider_candidate(
        &self,
        process_tree: &ProcessTree,
        client_process_id: u32,
    ) -> bool {
        match self.surface.as_ref() {
            Some(OuterTerminalSurface::Ghostty { .. }) => true,
            None => self.terminal_tty.as_ref().is_some_and(|expected| {
                process_tree.ttys.get(&client_process_id) == Some(expected)
            }),
        }
    }

    fn fingerprint_material(&self) -> String {
        match self.surface.as_ref() {
            Some(OuterTerminalSurface::Ghostty { terminal_id, .. }) => {
                format!("{}\u{001f}ghostty\u{001f}{terminal_id}", self.process_id)
            }
            None => format!(
                "{}\u{001f}{}",
                self.process_id,
                self.terminal_tty.as_deref().unwrap_or_default()
            ),
        }
    }
}

#[derive(Debug, Clone)]
struct ProviderClientGuard {
    outer: OuterTerminalGuard,
    process_id: u32,
    provider: &'static str,
}

impl ProviderClientGuard {
    fn bind(
        outer: OuterTerminalGuard,
        process_tree: &ProcessTree,
        process_id: u32,
        provider: &'static str,
    ) -> Result<Self, String> {
        if !process_tree.is_descendant_of(process_id, outer.process_id)
            || process_tree.unique_tty_provider_process(outer.process_id, provider)
                != Some(process_id)
            || !outer.accepts_provider_candidate(process_tree, process_id)
        {
            return Err("outer_terminal_provider_unmatched".to_owned());
        }
        Ok(Self {
            outer,
            process_id,
            provider,
        })
    }

    fn verify(&self, runner: &dyn CommandRunner) -> Result<(), String> {
        self.outer.verify(runner)?;
        let process_tree = ProcessTree::collect(runner)?;
        if !process_tree.is_descendant_of(self.process_id, self.outer.process_id)
            || process_tree.unique_tty_provider_process(self.outer.process_id, self.provider)
                != Some(self.process_id)
        {
            return Err("outer_terminal_provider_changed".to_owned());
        }
        Ok(())
    }

    fn fingerprint_material(&self) -> String {
        self.outer.fingerprint_material()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct GhosttyTerminalIdentity {
    frontmost_process_id: u32,
    terminal_id: String,
}

fn is_ghostty(target: &FrontmostTarget) -> bool {
    target
        .bundle_id
        .as_deref()
        .is_some_and(|bundle_id| bundle_id.eq_ignore_ascii_case("com.mitchellh.ghostty"))
        || target
            .name
            .as_deref()
            .is_some_and(|name| name.eq_ignore_ascii_case("ghostty"))
}

fn read_frontmost_process_id(runner: &dyn CommandRunner) -> Result<u32, String> {
    let output = runner.run(
        Path::new(OSASCRIPT_PROGRAM),
        &[
            OsString::from("-e"),
            OsString::from(FRONTMOST_PROCESS_SCRIPT),
        ],
    )?;
    output
        .stdout_text()
        .trim()
        .parse::<u32>()
        .ok()
        .filter(|process_id| *process_id > 0)
        .ok_or_else(|| "frontmost_process_invalid".to_owned())
}

fn read_ghostty_focused_terminal(
    runner: &dyn CommandRunner,
) -> Result<GhosttyTerminalIdentity, String> {
    const SCRIPT: &str = r#"
tell application "Ghostty"
  if (count of windows) is 0 then return ""
  tell application "System Events"
    set frontProcessId to (unix id of first application process whose frontmost is true) as text
  end tell
  set focusedTerminal to focused terminal of selected tab of front window
  set terminalId to (id of focusedTerminal as text)
  return frontProcessId & linefeed & terminalId
end tell
"#;
    let output = runner.run(
        Path::new(OSASCRIPT_PROGRAM),
        &[OsString::from("-e"), OsString::from(SCRIPT)],
    )?;
    let text = output.stdout_text();
    let mut lines = text.lines();
    let frontmost_process_id = lines.next().and_then(|value| value.trim().parse().ok());
    let terminal_id = lines.next().unwrap_or_default().trim().to_owned();
    if lines.next().is_some()
        || terminal_id.is_empty()
        || terminal_id.len() > 256
        || frontmost_process_id.is_none()
    {
        return Err("ghostty_terminal_id_invalid".to_owned());
    }
    Ok(GhosttyTerminalIdentity {
        frontmost_process_id: frontmost_process_id.expect("validated above"),
        terminal_id,
    })
}

#[derive(Debug)]
struct CommandOutput {
    stdout: Vec<u8>,
}

impl CommandOutput {
    fn stdout_text(&self) -> String {
        String::from_utf8_lossy(&self.stdout).into_owned()
    }
}

#[derive(Debug, Default)]
struct SystemCommandRunner {
    deadline: Option<Instant>,
    cancellation: Option<Arc<AtomicBool>>,
}

impl SystemCommandRunner {
    fn for_discovery(deadline: Instant, cancellation: Arc<AtomicBool>) -> Self {
        Self {
            deadline: Some(deadline),
            cancellation: Some(cancellation),
        }
    }
}

impl CommandRunner for SystemCommandRunner {
    fn run(&self, program: &Path, arguments: &[OsString]) -> Result<CommandOutput, String> {
        if self.cancelled() {
            return Err("command_cancelled".to_owned());
        }
        let now = Instant::now();
        let deadline = self.deadline.map_or(now + COMMAND_TIMEOUT, |overall| {
            overall.min(now + COMMAND_TIMEOUT)
        });
        if deadline <= now {
            return Err("command_cancelled".to_owned());
        }
        let mut child = Command::new(program)
            .args(arguments)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|error| format!("spawn_failed:{error}"))?;
        let mut stdout = child
            .stdout
            .take()
            .ok_or_else(|| "stdout_unavailable".to_owned())?;
        let mut stderr = child
            .stderr
            .take()
            .ok_or_else(|| "stderr_unavailable".to_owned())?;
        let stdout_reader = std::thread::spawn(move || {
            let mut bytes = Vec::new();
            stdout.read_to_end(&mut bytes).map(|_| bytes)
        });
        let stderr_reader = std::thread::spawn(move || {
            let mut bytes = Vec::new();
            stderr.read_to_end(&mut bytes).map(|_| bytes)
        });
        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break Some(status),
                Ok(None) if Instant::now() < deadline && !self.cancelled() => {
                    std::thread::sleep(Duration::from_millis(10));
                }
                Ok(None) | Err(_) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    break None;
                }
            }
        };
        let stdout = stdout_reader
            .join()
            .map_err(|_| "stdout_reader_panicked".to_owned())?
            .map_err(|error| format!("stdout_read_failed:{error}"))?;
        let _stderr = stderr_reader
            .join()
            .map_err(|_| "stderr_reader_panicked".to_owned())?
            .map_err(|error| format!("stderr_read_failed:{error}"))?;
        let status = status.ok_or_else(|| {
            if self.cancelled() {
                "command_cancelled".to_owned()
            } else {
                "command_timed_out".to_owned()
            }
        })?;
        if !status.success() {
            return Err("command_failed".to_owned());
        }
        Ok(CommandOutput { stdout })
    }

    fn cancelled(&self) -> bool {
        self.deadline
            .is_some_and(|deadline| Instant::now() >= deadline)
            || self
                .cancellation
                .as_ref()
                .is_some_and(|cancelled| cancelled.load(Ordering::Acquire))
    }
}

#[derive(Debug, Default)]
struct ProcessTree {
    parents: HashMap<u32, u32>,
    ttys: HashMap<u32, String>,
    commands: HashMap<u32, String>,
}

impl ProcessTree {
    fn collect(runner: &dyn CommandRunner) -> Result<Self, String> {
        let output = runner.run(
            Path::new(PROCESS_LIST_PROGRAM),
            &[
                OsString::from("-axo"),
                OsString::from("pid=,ppid=,tty=,comm="),
            ],
        )?;
        Ok(Self::parse(&output.stdout_text()))
    }

    fn parse(output: &str) -> Self {
        let mut parents = HashMap::new();
        let mut ttys = HashMap::new();
        let mut commands = HashMap::new();
        for line in output.lines() {
            let mut fields = line.split_whitespace();
            let Some(process_id) = fields.next().and_then(|value| value.parse().ok()) else {
                continue;
            };
            let Some(parent_id) = fields.next().and_then(|value| value.parse().ok()) else {
                continue;
            };
            let Some(tty) = fields.next() else {
                continue;
            };
            parents.insert(process_id, parent_id);
            if tty != "??" && tty != "?" && tty != "-" {
                ttys.insert(process_id, tty.to_owned());
            }
            commands.insert(process_id, fields.collect::<Vec<_>>().join(" "));
        }
        Self {
            parents,
            ttys,
            commands,
        }
    }

    fn is_descendant_of(&self, process_id: u32, ancestor_id: u32) -> bool {
        if process_id == ancestor_id {
            return false;
        }
        let mut current = process_id;
        for _ in 0..self.parents.len() {
            let Some(parent) = self.parents.get(&current).copied() else {
                return false;
            };
            if parent == ancestor_id {
                return true;
            }
            if parent == 0 || parent == current {
                return false;
            }
            current = parent;
        }
        false
    }

    fn descendants_of(&self, ancestor_id: u32) -> Vec<u32> {
        self.parents
            .keys()
            .copied()
            .filter(|process_id| self.is_descendant_of(*process_id, ancestor_id))
            .collect()
    }

    fn single_terminal_tty(&self, ancestor_id: u32) -> Option<String> {
        let ttys = self
            .descendants_of(ancestor_id)
            .into_iter()
            .filter_map(|process_id| self.ttys.get(&process_id).cloned())
            .collect::<HashSet<_>>();
        match ttys.into_iter().collect::<Vec<_>>().as_slice() {
            [single] => Some(single.clone()),
            _ => None,
        }
    }

    fn unique_tty_provider_process(&self, ancestor_id: u32, provider: &str) -> Option<u32> {
        match self
            .tty_provider_processes(ancestor_id, provider)
            .as_slice()
        {
            [single] => Some(*single),
            _ => None,
        }
    }

    fn tty_provider_processes(&self, ancestor_id: u32, provider: &str) -> Vec<u32> {
        let mut matches = self
            .descendants_of(ancestor_id)
            .into_iter()
            .filter(|process_id| self.ttys.contains_key(process_id))
            .filter(|process_id| {
                self.commands
                    .get(process_id)
                    .is_some_and(|command| command_is_provider(command, provider))
            })
            .collect::<Vec<_>>();
        matches.sort_unstable();
        matches
    }
}

fn command_is_provider(command: &str, provider: &str) -> bool {
    let basename = Path::new(command.trim_start_matches('-'))
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(command)
        .to_ascii_lowercase();
    let provider = provider.to_ascii_lowercase();
    basename == provider
        || basename.starts_with(&format!("{provider}:"))
        || basename.starts_with(&format!("{provider}-"))
}

pub(crate) struct PaneDiscoveryTarget {
    outer_process_id: u32,
    process_tree: ProcessTree,
    outer_guard: OuterTerminalGuard,
    deadline: Instant,
    cancellation: Arc<AtomicBool>,
}

/// Capture the outer terminal surface synchronously at dictation start.
///
/// Provider probing stays in the background, but it can never silently retarget
/// to another tab or split in the same terminal process.
pub(crate) fn capture_pane_target(
    target: &FrontmostTarget,
    deadline: Instant,
    cancellation: Arc<AtomicBool>,
) -> Option<PaneDiscoveryTarget> {
    if !looks_like_terminal(target) {
        return None;
    }
    let outer_process_id = target.process_id?;
    let runner = SystemCommandRunner::for_discovery(deadline, cancellation.clone());
    let process_tree = ProcessTree::collect(&runner).ok()?;
    let outer_guard = OuterTerminalGuard::capture(target, &process_tree, &runner).ok()?;
    Some(PaneDiscoveryTarget {
        outer_process_id,
        process_tree,
        outer_guard,
        deadline,
        cancellation,
    })
}

/// Find the innermost safe pane API associated with the captured terminal.
///
/// Herdr is preferred because its rendered buffer already includes any nested
/// terminal UI. tmux and Zellij are used when their clients can be
/// proven to belong to the same outer terminal process.
pub(crate) fn identify_pane(target: PaneDiscoveryTarget) -> Result<Option<LockedPane>, String> {
    let runner = SystemCommandRunner::for_discovery(target.deadline, target.cancellation);
    if runner.cancelled() {
        return Err("pane_discovery_cancelled".to_owned());
    }

    match target
        .process_tree
        .tty_provider_processes(target.outer_process_id, "herdr")
        .as_slice()
    {
        [client_process_id] => {
            return identify_herdr(
                &target.process_tree,
                target.outer_guard.clone(),
                *client_process_id,
                &runner,
            )
            .map(Some);
        }
        [] => {}
        _ => return Err("herdr_client_ambiguous".to_owned()),
    }

    Ok(target
        .process_tree
        .unique_tty_provider_process(target.outer_process_id, "tmux")
        .and_then(|_| identify_tmux(&target.process_tree, target.outer_guard.clone(), &runner))
        .or_else(|| {
            target
                .process_tree
                .unique_tty_provider_process(target.outer_process_id, "zellij")
                .and_then(|_| {
                    identify_zellij(
                        target.outer_process_id,
                        &target.process_tree,
                        target.outer_guard,
                        &runner,
                    )
                })
        }))
}

fn looks_like_terminal(target: &FrontmostTarget) -> bool {
    let identity = format!(
        "{} {}",
        target.bundle_id.as_deref().unwrap_or_default(),
        target.name.as_deref().unwrap_or_default()
    )
    .to_ascii_lowercase();
    [
        "terminal",
        "ghostty",
        "iterm",
        "wezterm",
        "kitty",
        "alacritty",
        "warp",
        "herdr",
    ]
    .iter()
    .any(|candidate| identity.contains(candidate))
}

fn resolve_executable(name: &str) -> Option<PathBuf> {
    let mut directories = std::env::var_os("PATH")
        .map(|path| std::env::split_paths(&path).collect::<Vec<_>>())
        .unwrap_or_default();
    directories.extend([
        PathBuf::from("/opt/homebrew/bin"),
        PathBuf::from("/usr/local/bin"),
        PathBuf::from("/usr/bin"),
    ]);
    if let Some(home) = std::env::var_os("HOME") {
        directories.push(PathBuf::from(home).join(".local/bin"));
    }
    directories
        .into_iter()
        .filter(|directory| directory.is_absolute())
        .map(|directory| directory.join(name))
        .find(|candidate| candidate.is_file())
}

#[derive(Debug, PartialEq, Eq)]
struct HerdrIdentity {
    pane_id: String,
    shell_pid: u32,
}

fn parse_herdr_identity(current: &str, process_info: &str) -> Option<HerdrIdentity> {
    let current: serde_json::Value = serde_json::from_str(current).ok()?;
    let process_info: serde_json::Value = serde_json::from_str(process_info).ok()?;
    let pane_id = current
        .pointer("/result/pane/pane_id")?
        .as_str()?
        .to_owned();
    let process_info = process_info.pointer("/result/process_info")?;
    if process_info.get("pane_id")?.as_str()? != pane_id {
        return None;
    }
    let shell_pid = u32::try_from(process_info.get("shell_pid")?.as_u64()?).ok()?;
    Some(HerdrIdentity { pane_id, shell_pid })
}

fn identify_herdr(
    process_tree: &ProcessTree,
    outer_guard: OuterTerminalGuard,
    client_process_id: u32,
    runner: &dyn CommandRunner,
) -> Result<LockedPane, String> {
    let executable =
        resolve_executable("herdr").ok_or_else(|| "herdr_executable_unavailable".to_owned())?;
    let current = runner
        .run(
            &executable,
            &[OsString::from("pane"), OsString::from("current")],
        )
        .map_err(|reason| format!("herdr_current_unavailable:{reason}"))?
        .stdout_text();
    let current_json: serde_json::Value =
        serde_json::from_str(&current).map_err(|_| "herdr_current_invalid".to_owned())?;
    let pane_id = current_json
        .pointer("/result/pane/pane_id")
        .and_then(|value| value.as_str())
        .ok_or_else(|| "herdr_current_pane_unavailable".to_owned())?
        .to_owned();
    let process_info = runner
        .run(
            &executable,
            &[
                OsString::from("pane"),
                OsString::from("process-info"),
                OsString::from("--pane"),
                OsString::from(&pane_id),
            ],
        )
        .map_err(|reason| format!("herdr_process_info_unavailable:{reason}"))?
        .stdout_text();
    let identity = parse_herdr_identity(&current, &process_info)
        .ok_or_else(|| "herdr_identity_invalid".to_owned())?;
    if !process_tree.is_descendant_of(identity.shell_pid, client_process_id) {
        return Err("herdr_shell_not_in_client_tree".to_owned());
    }
    let client_guard =
        ProviderClientGuard::bind(outer_guard, process_tree, client_process_id, "herdr")
            .map_err(|reason| format!("herdr_client_not_bound_to_outer_surface:{reason}"))?;
    Ok(LockedPane {
        inner: Arc::new(HerdrPane {
            executable,
            pane_id: identity.pane_id,
            client_guard,
            runner: Arc::new(SystemCommandRunner::default()),
        }),
    })
}

struct HerdrPane {
    executable: PathBuf,
    pane_id: String,
    client_guard: ProviderClientGuard,
    runner: Arc<dyn CommandRunner>,
}

impl PaneHandle for HerdrPane {
    fn observer_id(&self) -> &'static str {
        "herdr_pane_v1"
    }

    fn fingerprint_material(&self) -> String {
        format!(
            "herdr\u{001f}{}\u{001f}{}",
            self.pane_id,
            self.client_guard.fingerprint_material()
        )
    }

    fn snapshot(&self) -> Result<PaneSnapshot, String> {
        self.verify_provider()?;
        self.ensure_current_pane()?;
        let output = self.runner.run(
            &self.executable,
            &[
                OsString::from("pane"),
                OsString::from("read"),
                OsString::from(&self.pane_id),
                OsString::from("--source"),
                OsString::from("recent-unwrapped"),
                OsString::from("--lines"),
                OsString::from("80"),
                OsString::from("--format"),
                OsString::from("text"),
            ],
        )?;
        self.ensure_current_pane()?;
        self.verify_provider()?;
        Ok(parse_herdr_snapshot(&output.stdout_text()))
    }
}

impl HerdrPane {
    fn verify_provider(&self) -> Result<(), String> {
        self.client_guard.verify(self.runner.as_ref())
    }

    fn ensure_current_pane(&self) -> Result<(), String> {
        let output = self.runner.run(
            &self.executable,
            &[OsString::from("pane"), OsString::from("current")],
        )?;
        let value: serde_json::Value = serde_json::from_str(&output.stdout_text())
            .map_err(|_| "herdr_current_invalid".to_owned())?;
        match value
            .pointer("/result/pane/pane_id")
            .and_then(|id| id.as_str())
        {
            Some(pane_id) if pane_id == self.pane_id => Ok(()),
            _ => Err("herdr_focused_pane_changed".to_owned()),
        }
    }
}

fn parse_herdr_snapshot(output: &str) -> PaneSnapshot {
    PaneSnapshot {
        text: output.to_owned(),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct TmuxClient {
    process_id: u32,
    pane_id: String,
    session_id: String,
}

fn parse_tmux_clients(output: &str) -> Vec<TmuxClient> {
    output
        .lines()
        .filter_map(|line| {
            let mut fields = line.splitn(4, '\t');
            let _client_name = fields.next()?;
            let process_id = fields.next()?.parse().ok()?;
            let pane_id = fields.next()?.to_owned();
            let session_id = fields.next()?.to_owned();
            if !pane_id.starts_with('%') || session_id.is_empty() {
                return None;
            }
            Some(TmuxClient {
                process_id,
                pane_id,
                session_id,
            })
        })
        .collect()
}

fn identify_tmux(
    process_tree: &ProcessTree,
    outer_guard: OuterTerminalGuard,
    runner: &dyn CommandRunner,
) -> Option<LockedPane> {
    let executable = resolve_executable("tmux")?;
    let mut matches = Vec::new();
    for socket in tmux_socket_candidates(runner) {
        if runner.cancelled() {
            return None;
        }
        let mut arguments = Vec::new();
        if let Some(path) = socket.as_ref() {
            arguments.extend([OsString::from("-S"), path.as_os_str().to_owned()]);
        }
        arguments.extend([
            OsString::from("list-clients"),
            OsString::from("-F"),
            OsString::from("#{client_name}\t#{client_pid}\t#{pane_id}\t#{client_session}"),
        ]);
        let Ok(output) = runner.run(&executable, &arguments) else {
            continue;
        };
        for client in parse_tmux_clients(&output.stdout_text()) {
            if let Ok(client_guard) = ProviderClientGuard::bind(
                outer_guard.clone(),
                process_tree,
                client.process_id,
                "tmux",
            ) {
                matches.push((socket.clone(), client, client_guard));
            }
        }
    }
    matches.sort_by(|left, right| {
        (
            left.0.as_deref().unwrap_or_else(|| Path::new("")),
            left.1.process_id,
            left.1.pane_id.as_str(),
        )
            .cmp(&(
                right.0.as_deref().unwrap_or_else(|| Path::new("")),
                right.1.process_id,
                right.1.pane_id.as_str(),
            ))
    });
    matches.dedup_by(|left, right| {
        left.1.process_id == right.1.process_id && left.1.pane_id == right.1.pane_id
    });
    let (socket, client, client_guard) = match matches.as_slice() {
        [(socket, client, client_guard)] => (socket.clone(), client.clone(), client_guard.clone()),
        _ => return None,
    };
    Some(LockedPane {
        inner: Arc::new(TmuxPane {
            executable,
            socket,
            pane_id: client.pane_id,
            session_id: client.session_id,
            client_guard,
            runner: Arc::new(SystemCommandRunner::default()),
        }),
    })
}

fn tmux_socket_candidates(runner: &dyn CommandRunner) -> Vec<Option<PathBuf>> {
    let mut candidates = vec![None];
    let Ok(uid) = runner.run(Path::new("/usr/bin/id"), &[OsString::from("-u")]) else {
        return candidates;
    };
    let uid = uid.stdout_text().trim().to_owned();
    if uid.is_empty() {
        return candidates;
    }
    let mut seen = HashSet::new();
    for root in [Path::new("/tmp"), Path::new("/private/tmp")] {
        let directory = root.join(format!("tmux-{uid}"));
        let Ok(entries) = fs::read_dir(directory) else {
            continue;
        };
        for entry in entries.flatten() {
            if !entry.file_type().ok().is_some_and(file_type_is_socket) {
                continue;
            }
            let path = entry.path();
            if path.file_name().is_some_and(|name| name == "default") {
                continue;
            }
            if seen.insert(path.clone()) {
                candidates.push(Some(path));
            }
        }
    }
    candidates
}

#[cfg(unix)]
fn file_type_is_socket(file_type: fs::FileType) -> bool {
    file_type.is_socket()
}

#[cfg(not(unix))]
fn file_type_is_socket(_file_type: fs::FileType) -> bool {
    false
}

struct TmuxPane {
    executable: PathBuf,
    socket: Option<PathBuf>,
    pane_id: String,
    session_id: String,
    client_guard: ProviderClientGuard,
    runner: Arc<dyn CommandRunner>,
}

impl PaneHandle for TmuxPane {
    fn observer_id(&self) -> &'static str {
        "tmux_pane_v1"
    }

    fn fingerprint_material(&self) -> String {
        format!(
            "tmux\u{001f}{}\u{001f}{}\u{001f}{}\u{001f}{}",
            self.socket
                .as_deref()
                .map(Path::to_string_lossy)
                .unwrap_or_else(|| "default".into()),
            self.session_id,
            self.pane_id,
            self.client_guard.fingerprint_material()
        )
    }

    fn snapshot(&self) -> Result<PaneSnapshot, String> {
        self.ensure_client_still_targets_pane()?;
        let mut arguments = Vec::new();
        if let Some(path) = self.socket.as_ref() {
            arguments.extend([OsString::from("-S"), path.as_os_str().to_owned()]);
        }
        arguments.extend([
            OsString::from("capture-pane"),
            OsString::from("-p"),
            OsString::from("-J"),
            OsString::from("-t"),
            OsString::from(&self.pane_id),
        ]);
        let output = self.runner.run(&self.executable, &arguments)?;
        self.ensure_client_still_targets_pane()?;
        Ok(PaneSnapshot {
            text: output.stdout_text(),
        })
    }
}

impl TmuxPane {
    fn ensure_client_still_targets_pane(&self) -> Result<(), String> {
        let mut arguments = Vec::new();
        if let Some(path) = self.socket.as_ref() {
            arguments.extend([OsString::from("-S"), path.as_os_str().to_owned()]);
        }
        arguments.extend([
            OsString::from("list-clients"),
            OsString::from("-F"),
            OsString::from("#{client_name}\t#{client_pid}\t#{pane_id}\t#{client_session}"),
        ]);
        let output = self.runner.run(&self.executable, &arguments)?;
        let clients = parse_tmux_clients(&output.stdout_text());
        let _client = match clients
            .iter()
            .filter(|client| client.process_id == self.client_guard.process_id)
            .collect::<Vec<_>>()
            .as_slice()
        {
            [client] if client.pane_id == self.pane_id && client.session_id == self.session_id => {
                *client
            }
            _ => return Err("tmux_focused_pane_changed".to_owned()),
        };
        self.client_guard.verify(self.runner.as_ref())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ZellijClient {
    client_id: String,
    pane_id: String,
}

fn parse_zellij_clients(output: &str) -> Vec<ZellijClient> {
    output
        .lines()
        .filter(|line| !line.trim_start().starts_with("CLIENT_ID"))
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            let client_id = fields.next()?.to_owned();
            let pane_id = fields.next()?.to_owned();
            if !(pane_id.starts_with("terminal_") || pane_id.starts_with("plugin_")) {
                return None;
            }
            Some(ZellijClient { client_id, pane_id })
        })
        .collect()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ZellijSnapshotMode {
    TargetedPaneStdout,
}

fn zellij_snapshot_mode(version: &str) -> Option<ZellijSnapshotMode> {
    let version = version.split_whitespace().find(|part| {
        part.chars()
            .next()
            .is_some_and(|value| value.is_ascii_digit())
    })?;
    let mut parts = version
        .split('.')
        .filter_map(|part| part.parse::<u32>().ok());
    let version = (
        parts.next().unwrap_or_default(),
        parts.next().unwrap_or_default(),
        parts.next().unwrap_or_default(),
    );
    if version >= (0, 44, 0) {
        Some(ZellijSnapshotMode::TargetedPaneStdout)
    } else {
        None
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ZellijPaneMetadata {
    pane_id: String,
}

fn parse_zellij_panes(output: &str) -> Vec<ZellijPaneMetadata> {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(output) else {
        return Vec::new();
    };
    value
        .as_array()
        .into_iter()
        .flatten()
        .filter(|pane| pane.get("is_plugin").and_then(|value| value.as_bool()) == Some(false))
        .filter_map(|pane| {
            let pane_id = match pane.get("id")? {
                serde_json::Value::Number(id) => format!("terminal_{}", id.as_u64()?),
                serde_json::Value::String(id) if id.starts_with("terminal_") => id.clone(),
                _ => return None,
            };
            Some(ZellijPaneMetadata { pane_id })
        })
        .collect()
}

fn zellij_list_panes(
    executable: &Path,
    session: &str,
    runner: &dyn CommandRunner,
) -> Result<Vec<ZellijPaneMetadata>, String> {
    let output = runner.run(
        executable,
        &[
            OsString::from("--session"),
            OsString::from(session),
            OsString::from("action"),
            OsString::from("list-panes"),
            OsString::from("--json"),
        ],
    )?;
    let panes = parse_zellij_panes(&output.stdout_text());
    if panes.is_empty() {
        return Err("zellij_pane_metadata_unavailable".to_owned());
    }
    Ok(panes)
}

fn identify_zellij(
    outer_process_id: u32,
    process_tree: &ProcessTree,
    outer_guard: OuterTerminalGuard,
    runner: &dyn CommandRunner,
) -> Option<LockedPane> {
    let executable = resolve_executable("zellij")?;
    let version = runner
        .run(&executable, &[OsString::from("--version")])
        .ok()?
        .stdout_text();
    let snapshot_mode = match zellij_snapshot_mode(&version) {
        Some(mode) => mode,
        None => {
            tracing::info!(
                version = version.trim(),
                "Zellij pane observation requires Zellij 0.44 or newer; using Accessibility fallback"
            );
            return None;
        }
    };
    let client_process_id = process_tree.unique_tty_provider_process(outer_process_id, "zellij")?;
    let zellij_processes = process_tree
        .descendants_of(outer_process_id)
        .into_iter()
        .filter(|process_id| {
            process_tree
                .commands
                .get(process_id)
                .is_some_and(|command| command.to_ascii_lowercase().contains("zellij"))
        })
        .collect::<Vec<_>>();
    if zellij_processes.is_empty() {
        return None;
    }
    let mut candidate_processes = HashSet::new();
    for zellij_process in zellij_processes {
        candidate_processes.insert(zellij_process);
        candidate_processes.extend(process_tree.descendants_of(zellij_process));
    }
    let process_ids = candidate_processes
        .into_iter()
        .map(|process_id| process_id.to_string())
        .collect::<Vec<_>>()
        .join(",");
    let mut sessions = HashSet::new();
    if !process_ids.is_empty() {
        let output = runner.run(
            Path::new(PROCESS_LIST_PROGRAM),
            &[
                OsString::from("eww"),
                OsString::from("-p"),
                OsString::from(process_ids),
                OsString::from("-o"),
                OsString::from("command="),
            ],
        );
        if let Ok(output) = output {
            for process in output.stdout_text().lines() {
                if let Some(session) = extract_process_environment(process, "ZELLIJ_SESSION_NAME") {
                    sessions.insert(session);
                }
            }
        }
    }
    let discovered_sessions = sessions.into_iter().collect::<Vec<_>>();
    let session = match discovered_sessions.as_slice() {
        [single] => single.clone(),
        [] => {
            let listed = runner
                .run(
                    &executable,
                    &[
                        OsString::from("list-sessions"),
                        OsString::from("--short"),
                        OsString::from("--no-formatting"),
                    ],
                )
                .ok()?;
            match parse_zellij_sessions(&listed.stdout_text()).as_slice() {
                [single] => single.clone(),
                _ => return None,
            }
        }
        _ => return None,
    };
    let clients = runner
        .run(
            &executable,
            &[
                OsString::from("--session"),
                OsString::from(&session),
                OsString::from("action"),
                OsString::from("list-clients"),
            ],
        )
        .ok()?;
    let parsed_clients = parse_zellij_clients(&clients.stdout_text());
    let client = match parsed_clients.as_slice() {
        [single] if single.pane_id.starts_with("terminal_") => single.clone(),
        _ => return None,
    };
    let pane_metadata = zellij_list_panes(&executable, &session, runner)
        .ok()?
        .into_iter()
        .filter(|pane| pane.pane_id == client.pane_id)
        .collect::<Vec<_>>();
    if !matches!(pane_metadata.as_slice(), [_]) {
        return None;
    }
    let client_guard =
        ProviderClientGuard::bind(outer_guard, process_tree, client_process_id, "zellij").ok()?;
    Some(LockedPane {
        inner: Arc::new(ZellijPane {
            executable,
            session,
            client_id: client.client_id,
            pane_id: client.pane_id,
            snapshot_mode,
            client_guard,
            runner: Arc::new(SystemCommandRunner::default()),
        }),
    })
}

fn parse_zellij_sessions(output: &str) -> Vec<String> {
    output
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_owned)
        .collect()
}

fn extract_process_environment(output: &str, key: &str) -> Option<String> {
    let marker = format!(" {key}=");
    let value = output.split_once(&marker)?.1;
    value
        .split_whitespace()
        .next()
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
}

struct ZellijPane {
    executable: PathBuf,
    session: String,
    client_id: String,
    pane_id: String,
    snapshot_mode: ZellijSnapshotMode,
    client_guard: ProviderClientGuard,
    runner: Arc<dyn CommandRunner>,
}

impl PaneHandle for ZellijPane {
    fn observer_id(&self) -> &'static str {
        "zellij_pane_v1"
    }

    fn fingerprint_material(&self) -> String {
        format!(
            "zellij\u{001f}{}\u{001f}{}\u{001f}{}",
            self.session,
            self.pane_id,
            self.client_guard.fingerprint_material()
        )
    }

    fn snapshot(&self) -> Result<PaneSnapshot, String> {
        match self.snapshot_mode {
            ZellijSnapshotMode::TargetedPaneStdout => {
                self.ensure_only_client_still_targets_pane()?;
                let output = self.runner.run(
                    &self.executable,
                    &[
                        OsString::from("--session"),
                        OsString::from(&self.session),
                        OsString::from("action"),
                        OsString::from("dump-screen"),
                        OsString::from("--pane-id"),
                        OsString::from(&self.pane_id),
                    ],
                )?;
                self.ensure_only_client_still_targets_pane()?;
                Ok(PaneSnapshot {
                    text: output.stdout_text(),
                })
            }
        }
    }
}

impl ZellijPane {
    fn verify_provider(&self) -> Result<(), String> {
        let matches = zellij_list_panes(&self.executable, &self.session, self.runner.as_ref())?
            .into_iter()
            .filter(|pane| pane.pane_id == self.pane_id)
            .collect::<Vec<_>>();
        match matches.as_slice() {
            [_] => self.client_guard.verify(self.runner.as_ref()),
            _ => Err("zellij_pane_metadata_changed".to_owned()),
        }
    }

    fn ensure_only_client_still_targets_pane(&self) -> Result<(), String> {
        let output = self.runner.run(
            &self.executable,
            &[
                OsString::from("--session"),
                OsString::from(&self.session),
                OsString::from("action"),
                OsString::from("list-clients"),
            ],
        )?;
        match parse_zellij_clients(&output.stdout_text()).as_slice() {
            [client] if client.client_id == self.client_id && client.pane_id == self.pane_id => {
                self.verify_provider()
            }
            _ => Err("zellij_focused_pane_changed".into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct StaticSystemRunner {
        ghostty_identity: &'static str,
        process_list: &'static str,
    }

    impl CommandRunner for StaticSystemRunner {
        fn run(&self, program: &Path, _arguments: &[OsString]) -> Result<CommandOutput, String> {
            let output = if program == Path::new(OSASCRIPT_PROGRAM) {
                self.ghostty_identity
            } else if program == Path::new(PROCESS_LIST_PROGRAM) {
                self.process_list
            } else {
                return Err("unexpected_test_command".to_owned());
            };
            Ok(CommandOutput {
                stdout: output.as_bytes().to_vec(),
            })
        }
    }

    struct ConstantOutputRunner(&'static str);

    impl CommandRunner for ConstantOutputRunner {
        fn run(&self, _program: &Path, _arguments: &[OsString]) -> Result<CommandOutput, String> {
            Ok(CommandOutput {
                stdout: self.0.as_bytes().to_vec(),
            })
        }
    }

    fn single_herdr_process_list() -> &'static str {
        "659 1 ?? /Applications/Ghostty.app/Contents/MacOS/ghostty\n\
         755 659 ttys030 /usr/bin/login\n\
         762 755 ttys030 -/bin/zsh\n\
         19794 762 ttys030 herdr\n"
    }

    fn ghostty_outer_guard() -> OuterTerminalGuard {
        OuterTerminalGuard {
            process_id: 659,
            surface: Some(OuterTerminalSurface::Ghostty {
                terminal_id: "terminal-1".into(),
            }),
            terminal_tty: None,
        }
    }

    fn bound_herdr_client_guard() -> ProviderClientGuard {
        let tree = ProcessTree::parse(single_herdr_process_list());
        ProviderClientGuard::bind(ghostty_outer_guard(), &tree, 19794, "herdr").unwrap()
    }

    #[test]
    fn pane_process_must_belong_to_the_frontmost_terminal_tree() {
        let tree = ProcessTree::parse(
            "659 1 ?? /Applications/Ghostty.app/Contents/MacOS/ghostty\n\
             755 659 ttys030 /usr/bin/login\n\
             762 755 ttys030 -/bin/zsh\n\
             19794 762 ttys030 herdr\n\
             19796 19794 ttys030 herdr-server\n\
             19799 19796 ttys030 -zsh\n\
             88000 1 ttys099 unrelated\n",
        );

        assert!(tree.is_descendant_of(19799, 659));
        assert!(!tree.is_descendant_of(88000, 659));
        assert!(!tree.is_descendant_of(659, 659));
        assert_eq!(tree.single_terminal_tty(659).as_deref(), Some("ttys030"));
    }

    #[test]
    fn herdr_identity_uses_pane_id_and_shell_pid_from_cli_envelopes() {
        let identity = parse_herdr_identity(
            r#"{"result":{"pane":{"pane_id":"w7:p2","revision":27},"type":"pane_current"}}"#,
            r#"{"result":{"process_info":{"pane_id":"w7:p2","shell_pid":19799,"foreground_processes":[{"pid":19799,"cwd":"/work","name":"zsh"}]},"type":"pane_process_info"}}"#,
        );

        assert_eq!(
            identity,
            Some(HerdrIdentity {
                pane_id: "w7:p2".into(),
                shell_pid: 19799,
            })
        );
    }

    #[test]
    fn herdr_binding_does_not_conflate_outer_and_inner_working_directories() {
        let identity = parse_herdr_identity(
            r#"{"result":{"pane":{"pane_id":"w7:p2"}}}"#,
            r#"{"result":{"process_info":{"pane_id":"w7:p2","shell_pid":19799,"foreground_processes":[{"pid":19799,"cwd":"/current-herdr-workspace","name":"zsh"},{"pid":19800,"cwd":"/a-child-process-directory","name":"codex"}]}}}"#,
        )
        .expect("Herdr pane identity must not depend on mutable working directories");
        let tree = ProcessTree::parse(
            "659 1 ?? /Applications/Ghostty.app/Contents/MacOS/ghostty\n\
             755 659 ttys030 /usr/bin/login\n\
             762 755 ttys030 -/bin/zsh\n\
             19794 762 ttys030 herdr\n\
             19796 19794 ?? herdr-server\n\
             19799 19796 ?? -zsh\n",
        );
        let outer_guard = OuterTerminalGuard {
            process_id: 659,
            surface: Some(OuterTerminalSurface::Ghostty {
                terminal_id: "terminal-1".into(),
            }),
            terminal_tty: None,
        };

        assert!(tree.is_descendant_of(identity.shell_pid, 19794));
        assert!(ProviderClientGuard::bind(outer_guard, &tree, 19794, "herdr").is_ok());
    }

    #[test]
    fn provider_guard_uses_stable_surface_id_after_binding() {
        let client_guard = bound_herdr_client_guard();
        let runner = StaticSystemRunner {
            ghostty_identity: "659\nterminal-1\n",
            process_list: single_herdr_process_list(),
        };

        assert!(client_guard.verify(&runner).is_ok());
    }

    #[test]
    fn non_ghostty_provider_guard_binds_and_verifies_with_the_same_uniqueness_rule() {
        let process_tree = ProcessTree::parse(single_herdr_process_list());
        let outer_guard = OuterTerminalGuard {
            process_id: 659,
            surface: None,
            terminal_tty: Some("ttys030".into()),
        };
        let client_guard =
            ProviderClientGuard::bind(outer_guard.clone(), &process_tree, 19794, "herdr")
                .expect("a unique provider on the captured tty must bind");
        let runner = StaticSystemRunner {
            ghostty_identity: "659\n",
            process_list: single_herdr_process_list(),
        };

        assert!(client_guard.verify(&runner).is_ok());

        let ambiguous_tree = ProcessTree::parse(
            "659 1 ?? /Applications/Terminal.app/Contents/MacOS/Terminal\n\
             755 659 ttys030 /usr/bin/login\n\
             762 755 ttys030 -/bin/zsh\n\
             19794 762 ttys030 herdr\n\
             756 659 ttys031 /usr/bin/login\n\
             763 756 ttys031 -/bin/zsh\n\
             19795 763 ttys031 herdr\n",
        );
        assert_eq!(
            ProviderClientGuard::bind(outer_guard, &ambiguous_tree, 19794, "herdr")
                .expect_err("an ambiguous provider must fail during binding"),
            "outer_terminal_provider_unmatched"
        );
    }

    #[test]
    fn provider_guard_rejects_a_different_outer_surface() {
        let client_guard = bound_herdr_client_guard();
        let runner = StaticSystemRunner {
            ghostty_identity: "659\nterminal-2\n",
            process_list: single_herdr_process_list(),
        };

        assert_eq!(
            client_guard.verify(&runner),
            Err("outer_terminal_surface_changed".to_owned())
        );
    }

    #[test]
    fn provider_guard_rejects_a_replaced_client_process() {
        let client_guard = bound_herdr_client_guard();
        let runner = StaticSystemRunner {
            ghostty_identity: "659\nterminal-1\n",
            process_list: "659 1 ?? /Applications/Ghostty.app/Contents/MacOS/ghostty\n\
                 755 659 ttys030 /usr/bin/login\n\
                 762 755 ttys030 -/bin/zsh\n\
                 19795 762 ttys030 herdr\n",
        };

        assert_eq!(
            client_guard.verify(&runner),
            Err("outer_terminal_provider_changed".to_owned())
        );
    }

    #[test]
    fn herdr_guard_rejects_a_different_current_pane() {
        let pane = HerdrPane {
            executable: PathBuf::from("herdr"),
            pane_id: "w7:p2".into(),
            client_guard: ProviderClientGuard {
                outer: OuterTerminalGuard {
                    process_id: 659,
                    surface: None,
                    terminal_tty: Some("ttys030".into()),
                },
                process_id: 19794,
                provider: "herdr",
            },
            runner: Arc::new(ConstantOutputRunner(
                r#"{"result":{"pane":{"pane_id":"w7:p3"}}}"#,
            )),
        };

        assert_eq!(
            pane.ensure_current_pane(),
            Err("herdr_focused_pane_changed".to_owned())
        );
    }

    #[test]
    fn ambiguous_herdr_clients_fail_closed_before_other_provider_probes() {
        let process_tree = ProcessTree::parse(
            "659 1 ?? /Applications/Ghostty.app/Contents/MacOS/ghostty\n\
             755 659 ttys003 /usr/bin/login\n\
             762 755 ttys003 -/bin/zsh\n\
             19794 762 ttys003 herdr\n\
             756 659 ttys004 /usr/bin/login\n\
             763 756 ttys004 -/bin/zsh\n\
             19795 763 ttys004 herdr\n",
        );
        let target = PaneDiscoveryTarget {
            outer_process_id: 659,
            process_tree,
            outer_guard: OuterTerminalGuard {
                process_id: 659,
                surface: Some(OuterTerminalSurface::Ghostty {
                    terminal_id: "terminal-1".into(),
                }),
                terminal_tty: None,
            },
            deadline: Instant::now() + Duration::from_secs(1),
            cancellation: Arc::new(AtomicBool::new(false)),
        };

        assert_eq!(
            identify_pane(target).err().as_deref(),
            Some("herdr_client_ambiguous")
        );
    }

    #[test]
    fn herdr_snapshot_returns_only_the_rendered_text() {
        let snapshot = parse_herdr_snapshot("prompt HERDR\n");

        assert_eq!(snapshot.text, "prompt HERDR\n");
    }

    #[test]
    fn tmux_client_format_uses_only_process_pane_and_session_identity() {
        let clients = parse_tmux_clients("/dev/ttys001\t421\t%7\twork\n");

        assert_eq!(
            clients,
            vec![TmuxClient {
                process_id: 421,
                pane_id: "%7".into(),
                session_id: "work".into(),
            }]
        );
    }

    #[test]
    fn zellij_client_table_exposes_the_focused_pane() {
        let clients = parse_zellij_clients(
            "CLIENT_ID ZELLIJ_PANE_ID RUNNING_COMMAND\n\
             1 terminal_3 hx\n",
        );

        assert_eq!(
            clients,
            vec![ZellijClient {
                client_id: "1".into(),
                pane_id: "terminal_3".into(),
            }]
        );
    }

    #[test]
    fn zellij_requires_the_version_with_stable_pane_targeting() {
        assert_eq!(zellij_snapshot_mode("zellij 0.42.2"), None);
        assert_eq!(zellij_snapshot_mode("zellij 0.43.1"), None);
        assert_eq!(
            zellij_snapshot_mode("zellij 0.44.0"),
            Some(ZellijSnapshotMode::TargetedPaneStdout)
        );
        assert_eq!(
            zellij_snapshot_mode("zellij 1.0.0"),
            Some(ZellijSnapshotMode::TargetedPaneStdout)
        );
    }

    #[test]
    fn zellij_session_name_is_projected_without_other_environment_values() {
        let process = "zsh SECRET=do-not-project ZELLIJ_SESSION_NAME=work ZELLIJ_PANE_ID=3";

        assert_eq!(
            extract_process_environment(process, "ZELLIJ_SESSION_NAME"),
            Some("work".into())
        );
    }

    #[test]
    fn zellij_short_session_list_is_parsed_without_formatting_noise() {
        assert_eq!(
            parse_zellij_sessions("work\nnotes\n"),
            vec!["work".to_owned(), "notes".to_owned()]
        );
    }

    #[test]
    fn zellij_pane_metadata_does_not_require_a_mutable_title() {
        let panes = parse_zellij_panes(
            r#"[
              {"id":3,"is_plugin":false,"pane_cwd":"/work"},
              {"id":4,"is_plugin":true,"title":"status","pane_cwd":"/work"}
            ]"#,
        );

        assert_eq!(
            panes,
            vec![ZellijPaneMetadata {
                pane_id: "terminal_3".into(),
            }]
        );
    }

    #[test]
    fn provider_client_must_be_unique_across_outer_terminal_surfaces() {
        let one_client = ProcessTree::parse(
            "659 1 ?? /Applications/Ghostty.app/Contents/MacOS/ghostty\n\
             755 659 ttys003 /usr/bin/login\n\
             762 755 ttys003 -/bin/zsh\n\
             19794 762 ttys003 herdr\n\
             19796 19794 ?? /usr/local/bin/herdr\n",
        );
        assert_eq!(
            one_client.unique_tty_provider_process(659, "herdr"),
            Some(19794)
        );

        let two_clients = ProcessTree::parse(
            "659 1 ?? /Applications/Ghostty.app/Contents/MacOS/ghostty\n\
             755 659 ttys003 /usr/bin/login\n\
             762 755 ttys003 -/bin/zsh\n\
             19794 762 ttys003 herdr\n\
             756 659 ttys004 /usr/bin/login\n\
             763 756 ttys004 -/bin/zsh\n\
             19795 763 ttys004 herdr\n",
        );
        assert_eq!(two_clients.unique_tty_provider_process(659, "herdr"), None);
    }

    #[test]
    fn cancelled_discovery_never_starts_an_external_command() {
        let cancellation = Arc::new(AtomicBool::new(true));
        let runner = SystemCommandRunner::for_discovery(
            Instant::now() + Duration::from_secs(1),
            cancellation,
        );

        let error = runner
            .run(Path::new("/usr/bin/true"), &[])
            .expect_err("cancelled discovery");
        assert_eq!(error, "command_cancelled");
    }

    #[test]
    #[ignore = "requires Ghostty running a single Herdr client"]
    fn live_herdr_identity_binds_without_working_directory_equality() {
        let runner = SystemCommandRunner::default();
        let ghostty_process_id = String::from_utf8_lossy(
            &Command::new(OSASCRIPT_PROGRAM)
                .args([
                    "-e",
                    "tell application \"System Events\" to get unix id of application process \"Ghostty\"",
                ])
                .output()
                .expect("read Ghostty process")
                .stdout,
        )
        .trim()
        .parse::<u32>()
        .expect("valid Ghostty process id");
        let process_tree = ProcessTree::collect(&runner).expect("live process tree");
        let client_process_id = process_tree
            .unique_tty_provider_process(ghostty_process_id, "herdr")
            .expect("single Herdr client");
        let ghostty = read_ghostty_focused_terminal(&runner).expect("Ghostty surface metadata");
        let outer_guard = OuterTerminalGuard {
            process_id: ghostty_process_id,
            surface: Some(OuterTerminalSurface::Ghostty {
                terminal_id: ghostty.terminal_id,
            }),
            terminal_tty: None,
        };

        let pane = identify_herdr(&process_tree, outer_guard, client_process_id, &runner)
            .expect("Herdr identity must bind");

        assert_eq!(pane.observer_id(), "herdr_pane_v1");
    }

    #[test]
    #[ignore = "requires a frontmost Ghostty surface running a single Herdr client"]
    fn live_frontmost_herdr_surface_is_locked_before_reading() {
        let runner = SystemCommandRunner::default();
        let ghostty_process_id = String::from_utf8_lossy(
            &Command::new(OSASCRIPT_PROGRAM)
                .args([
                    "-e",
                    "tell application \"System Events\" to get unix id of application process \"Ghostty\"",
                ])
                .output()
                .expect("read Ghostty process")
                .stdout,
        )
            .trim()
            .parse::<u32>()
            .expect("valid Ghostty process id");
        assert_eq!(
            read_frontmost_process_id(&runner),
            Ok(ghostty_process_id),
            "Ghostty must be frontmost before the live probe"
        );
        let target = FrontmostTarget {
            name: Some("Ghostty".into()),
            bundle_id: Some("com.mitchellh.ghostty".into()),
            process_id: Some(ghostty_process_id),
        };
        let cancellation = Arc::new(AtomicBool::new(false));
        let discovery = capture_pane_target(
            &target,
            Instant::now() + Duration::from_secs(2),
            cancellation,
        )
        .expect("frontmost surface");
        let pane = identify_pane(discovery)
            .expect("Herdr discovery must be diagnosable")
            .expect("unambiguous Herdr pane");
        assert_eq!(pane.observer_id(), "herdr_pane_v1");
        assert!(!pane.snapshot().expect("Herdr snapshot").text.is_empty());
    }
}
