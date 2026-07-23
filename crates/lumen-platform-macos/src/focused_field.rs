//! Best-effort snapshot of the currently focused macOS Accessibility text field.

const FIELD_SEPARATOR: char = '\u{001e}';

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
    let output = std::process::Command::new("osascript")
        .arg("-e")
        .arg(script)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    parse_snapshot(&String::from_utf8_lossy(&output.stdout))
}

#[cfg(not(target_os = "macos"))]
pub fn focused_text_field_snapshot() -> Option<FocusedTextFieldSnapshot> {
    None
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
}
