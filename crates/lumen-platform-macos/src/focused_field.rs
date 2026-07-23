//! Best-effort snapshot of the currently focused macOS Accessibility text field.

#[cfg(target_os = "macos")]
use std::{
    io::Read,
    process::{Command, Stdio},
    time::{Duration, Instant},
};

const FIELD_SEPARATOR: char = '\u{001e}';
#[cfg(target_os = "macos")]
const FOCUSED_FIELD_SCRIPT_TIMEOUT: Duration = Duration::from_millis(750);

/// Accessibility value and stable identity material for the focused text control.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FocusedTextFieldSnapshot {
    pub value: String,
    pub role: String,
    pub subrole: String,
    pub identifier: String,
    pub x: String,
    pub y: String,
    pub width: String,
    pub height: String,
}

impl FocusedTextFieldSnapshot {
    /// Returns metadata suitable for hashing without including the field's text.
    pub fn fingerprint_material(&self) -> String {
        [
            self.role.as_str(),
            self.subrole.as_str(),
            self.identifier.as_str(),
            self.x.as_str(),
            self.y.as_str(),
            self.width.as_str(),
            self.height.as_str(),
        ]
        .join("\u{001f}")
    }
}

/// Reads the focused text control, returning `None` on denial, timeout, or parse failure.
#[cfg(target_os = "macos")]
pub fn focused_text_field_snapshot() -> Option<FocusedTextFieldSnapshot> {
    let script = r#"
set fieldSeparator to ASCII character 30
tell application "System Events"
  try
    set frontProc to first application process whose frontmost is true
    set focusedElement to value of attribute "AXFocusedUIElement" of frontProc
    set fieldRole to ""
    set fieldSubrole to ""
    set fieldIdentifier to ""
    set fieldX to ""
    set fieldY to ""
    set fieldWidth to ""
    set fieldHeight to ""
    set fieldValue to ""
    try
      set fieldRole to value of attribute "AXRole" of focusedElement as text
    end try
    try
      set fieldSubrole to value of attribute "AXSubrole" of focusedElement as text
    end try
    try
      set fieldIdentifier to value of attribute "AXIdentifier" of focusedElement as text
    end try
    try
      set fieldPosition to value of attribute "AXPosition" of focusedElement
      set fieldX to item 1 of fieldPosition as text
      set fieldY to item 2 of fieldPosition as text
    end try
    try
      set fieldSize to value of attribute "AXSize" of focusedElement
      set fieldWidth to item 1 of fieldSize as text
      set fieldHeight to item 2 of fieldSize as text
    end try
    try
      set fieldValue to value of attribute "AXValue" of focusedElement as text
    on error
      try
        set fieldValue to value of focusedElement as text
      on error
        try
          set fieldValue to value of attribute "AXSelectedText" of focusedElement as text
        end try
      end try
    end try
    return fieldRole & fieldSeparator & fieldSubrole & fieldSeparator & fieldIdentifier & fieldSeparator & fieldX & fieldSeparator & fieldY & fieldSeparator & fieldWidth & fieldSeparator & fieldHeight & fieldSeparator & fieldValue
  end try
end tell
return ""
"#;
    let stdout = run_osascript_with_timeout(script, FOCUSED_FIELD_SCRIPT_TIMEOUT)?;
    parse_snapshot(&String::from_utf8_lossy(&stdout))
}

#[cfg(not(target_os = "macos"))]
pub fn focused_text_field_snapshot() -> Option<FocusedTextFieldSnapshot> {
    None
}

#[cfg(target_os = "macos")]
fn run_osascript_with_timeout(script: &str, timeout: Duration) -> Option<Vec<u8>> {
    let mut child = Command::new("osascript")
        .arg("-e")
        .arg(script)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let mut stdout = child.stdout.take()?;
    let output_reader = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        stdout.read_to_end(&mut bytes).map(|_| bytes)
    });
    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(10));
            }
            Ok(None) | Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
        }
    };
    let stdout = output_reader.join().ok()?.ok()?;
    status.filter(|status| status.success()).map(|_| stdout)
}

fn parse_snapshot(output: &str) -> Option<FocusedTextFieldSnapshot> {
    let output = output.strip_suffix('\n').unwrap_or(output);
    let mut parts = output.splitn(8, FIELD_SEPARATOR);
    let role = parts.next()?.to_owned();
    let subrole = parts.next()?.to_owned();
    let identifier = parts.next()?.to_owned();
    let x = parts.next()?.to_owned();
    let y = parts.next()?.to_owned();
    let width = parts.next()?.to_owned();
    let height = parts.next()?.to_owned();
    let value = parts.next()?.to_owned();
    if role.is_empty() && value.is_empty() {
        return None;
    }
    Some(FocusedTextFieldSnapshot {
        value,
        role,
        subrole,
        identifier,
        x,
        y,
        width,
        height,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parser_preserves_multiline_unicode_field_values() {
        let separator = FIELD_SEPARATOR;
        let raw =
            format!("AXTextArea{separator}{separator}editor{separator}10{separator}20{separator}300{separator}80{separator}第一行\n第二行\n");

        let snapshot = parse_snapshot(&raw).unwrap();

        assert_eq!(snapshot.role, "AXTextArea");
        assert_eq!(snapshot.value, "第一行\n第二行");
        assert!(!snapshot.fingerprint_material().contains("第一行"));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn osascript_timeout_terminates_a_slow_probe() {
        let started = Instant::now();

        let output =
            run_osascript_with_timeout("delay 2\nreturn \"late\"", Duration::from_millis(30));

        assert!(output.is_none());
        assert!(started.elapsed() < Duration::from_secs(1));
    }
}
