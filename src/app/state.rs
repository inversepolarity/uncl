//! Floating tenant state persistence and initial viewport centering.
//!
//! Stores the tenant's last known geometry on disk so that restarting uncl
//! or respawning the tenant child restores the user's chosen location and size.
//! If no saved state is found (or when starting afresh), the tenant is centered
//! in the terminal viewport at default dimensions (80x25).

use crate::constants::{DEFAULT_HEIGHT, DEFAULT_WIDTH, MIN_HEIGHT, MIN_WIDTH};
use crate::geometry::Rect;
use std::path::PathBuf;

/// Calculate the centered rectangle for the floating tenant within `viewport` (cols, rows).
///
/// Spawns with dimensions 80 (width) x 25 (height), or clamped to the viewport if
/// the terminal is smaller than 80x25, positioned dead-center in the viewport.
pub fn centered_tenant_rect(viewport: (u16, u16)) -> Rect {
    let max_w = viewport.0.max(1);
    let max_h = viewport.1.max(1);

    let width = DEFAULT_WIDTH.min(max_w).max(MIN_WIDTH.min(max_w));
    let height = DEFAULT_HEIGHT.min(max_h).max(MIN_HEIGHT.min(max_h));

    let x = viewport.0.saturating_sub(width) / 2;
    let y = viewport.1.saturating_sub(height) / 2;

    Rect::new(x, y, width, height)
}

/// Determine the file path for saving/loading the floating tenant state.
///
/// Priority:
/// 1. `UNCL_STATE_FILE` environment variable
/// 2. `UNCL_STATE_DIR/tenant.json`
/// 3. Standard OS state path:
///    - Linux/Unix: `$XDG_STATE_HOME/uncl/tenant.json` or `$HOME/.local/state/uncl/tenant.json`
///    - macOS: `$XDG_STATE_HOME/uncl/tenant.json` or `$HOME/.local/state/uncl/tenant.json`
///    - Windows: `%LOCALAPPDATA%\uncl\tenant.json` or `%APPDATA%\uncl\tenant.json`
/// 4. Fallback: temporary directory (`$TMPDIR/uncl/tenant.json`)
pub fn state_file_path() -> Option<PathBuf> {
    if let Some(file) = std::env::var_os("UNCL_STATE_FILE") {
        if !file.is_empty() {
            return Some(PathBuf::from(file));
        }
    }

    if let Some(dir) = std::env::var_os("UNCL_STATE_DIR") {
        if !dir.is_empty() {
            return Some(PathBuf::from(dir).join("tenant.json"));
        }
    }

    if let Some(xdg) = std::env::var_os("XDG_STATE_HOME") {
        if !xdg.is_empty() {
            return Some(PathBuf::from(xdg).join("uncl").join("tenant.json"));
        }
    }

    #[cfg(windows)]
    {
        if let Some(appdata) =
            std::env::var_os("LOCALAPPDATA").or_else(|| std::env::var_os("APPDATA"))
        {
            if !appdata.is_empty() {
                return Some(PathBuf::from(appdata).join("uncl").join("tenant.json"));
            }
        }
        if let Some(userprofile) = std::env::var_os("USERPROFILE") {
            if !userprofile.is_empty() {
                return Some(
                    PathBuf::from(userprofile)
                        .join(".local")
                        .join("state")
                        .join("uncl")
                        .join("tenant.json"),
                );
            }
        }
    }

    #[cfg(not(windows))]
    {
        if let Some(home) = std::env::var_os("HOME") {
            if !home.is_empty() {
                return Some(
                    PathBuf::from(home)
                        .join(".local")
                        .join("state")
                        .join("uncl")
                        .join("tenant.json"),
                );
            }
        }
    }

    Some(std::env::temp_dir().join("uncl").join("tenant.json"))
}

/// Extract an unsigned integer value for a given JSON key from content.
fn extract_number(content: &str, key: &str) -> Option<u16> {
    let key_pos = content.find(key)?;
    let after_key = &content[key_pos + key.len()..];
    let colon_pos = after_key.find(':')?;
    let mut after_colon = after_key[colon_pos + 1..].trim_start();
    if after_colon.starts_with('"') {
        after_colon = &after_colon[1..];
    }
    let num_str: String = after_colon
        .chars()
        .take_while(|c| c.is_ascii_digit())
        .collect();
    if num_str.is_empty() {
        return None;
    }
    num_str.parse().ok()
}

/// Parse a `Rect` from JSON formatted content.
pub fn parse_geometry(content: &str) -> Option<Rect> {
    let x = extract_number(content, "\"x\"")?;
    let y = extract_number(content, "\"y\"")?;
    let width = extract_number(content, "\"width\"")?;
    let height = extract_number(content, "\"height\"")?;

    if width < MIN_WIDTH || height < MIN_HEIGHT {
        return None;
    }

    Some(Rect::new(x, y, width, height))
}

/// Load the last remembered floating tenant geometry.
///
/// Returns `None` if starting afresh (`UNCL_FRESH` is set), if the state file
/// does not exist, or if parsing fails.
pub fn load_tenant_geometry() -> Option<Rect> {
    if std::env::var_os("UNCL_FRESH").is_some() {
        return None;
    }

    let path = state_file_path()?;
    if !path.exists() {
        return None;
    }

    let content = std::fs::read_to_string(path).ok()?;
    parse_geometry(&content)
}

/// Persist the floating tenant geometry to disk.
pub fn save_tenant_geometry(rect: Rect) -> std::io::Result<()> {
    let Some(path) = state_file_path() else {
        return Ok(());
    };

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let json = format!(
        "{{\n  \"x\": {},\n  \"y\": {},\n  \"width\": {},\n  \"height\": {}\n}}\n",
        rect.x, rect.y, rect.width, rect.height
    );

    let tmp_path = path.with_extension(format!("tmp.{}", std::process::id()));
    std::fs::write(&tmp_path, json.as_bytes())?;
    if std::fs::rename(&tmp_path, &path).is_err() {
        let _ = std::fs::remove_file(&tmp_path);
        std::fs::write(&path, json.as_bytes())?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn centered_rect_computes_exact_middle_in_large_viewport() {
        let rect = centered_tenant_rect((120, 40));
        assert_eq!(rect.width, 82);
        assert_eq!(rect.height, 27);
        // x = (120 - 80) / 2 = 20
        assert_eq!(rect.x, 19);
        // y = (40 - 25) / 2 = 7
        assert_eq!(rect.y, 6);
        // Symmetrical margins
        assert_eq!(rect.x, 120 - rect.right());
        assert_eq!(rect.y, 40 - rect.bottom() - 1); // 7 on top, 8 on bottom (40 - 25 = 15)
    }

    #[test]
    fn centered_rect_in_tight_viewport() {
        let rect = centered_tenant_rect((80, 25));
        assert_eq!(rect.width, 80);
        assert_eq!(rect.height, 25);
        assert_eq!(rect.x, 0);
        assert_eq!(rect.y, 0);
    }

    #[test]
    fn centered_rect_in_small_viewport() {
        let rect = centered_tenant_rect((60, 20));
        assert_eq!(rect.width, 60);
        assert_eq!(rect.height, 20);
        assert_eq!(rect.x, 0);
        assert_eq!(rect.y, 0);
    }

    #[test]
    fn parse_geometry_round_trip() {
        let json = r#"{
  "x": 15,
  "y": 8,
  "width": 90,
  "height": 30
}"#;
        let rect = parse_geometry(json).expect("should parse valid json");
        assert_eq!(rect, Rect::new(15, 8, 90, 30));
    }

    #[test]
    fn parse_geometry_handles_quoted_numbers_and_spacing() {
        let json = r#"{"x": "10", "y" : 5 , "width": 80, "height": "25"}"#;
        let rect = parse_geometry(json).expect("should parse quoted numbers");
        assert_eq!(rect, Rect::new(10, 5, 80, 25));
    }

    #[test]
    fn parse_geometry_rejects_too_small_dimensions() {
        let json = r#"{"x": 0, "y": 0, "width": 5, "height": 25}"#;
        assert!(parse_geometry(json).is_none());

        let json_h = r#"{"x": 0, "y": 0, "width": 80, "height": 4}"#;
        assert!(parse_geometry(json_h).is_none());
    }

    #[test]
    fn parse_geometry_rejects_malformed_json() {
        assert!(parse_geometry("not json").is_none());
        assert!(parse_geometry(r#"{"x": 10}"#).is_none());
    }

    #[test]
    fn save_and_load_round_trip_with_state_file_env() {
        let tmp_dir = std::env::temp_dir().join(format!("uncl_test_{}", std::process::id()));
        let tmp_file = tmp_dir.join("test_tenant.json");
        unsafe {
            std::env::set_var("UNCL_STATE_FILE", &tmp_file);
        }

        let saved = Rect::new(12, 6, 85, 28);
        save_tenant_geometry(saved).expect("save should succeed");

        let loaded = load_tenant_geometry().expect("load should succeed");
        assert_eq!(loaded, saved);

        let _ = std::fs::remove_file(&tmp_file);
        let _ = std::fs::remove_dir(&tmp_dir);
        unsafe {
            std::env::remove_var("UNCL_STATE_FILE");
        }
    }
}
