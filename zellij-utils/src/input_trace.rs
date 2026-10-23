//! Opt-in sensitive input diagnostics from KudoLayton/zellij b14c8a3ff.
//! These logs can expose passwords and pasted secrets. Disabled by default.
const MAX_PREVIEW_BYTES: usize = 128;

pub fn enabled() -> bool {
    env_value_enabled(std::env::var("ZELLIJ_INPUT_TRACE").ok().as_deref())
}

pub fn env_value_enabled(value: Option<&str>) -> bool {
    value.is_some_and(|v| matches!(v.to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on"))
}

pub fn format_bytes(bytes: &[u8]) -> String {
    let preview = &bytes[..bytes.len().min(MAX_PREVIEW_BYTES)];
    let hex = preview
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<Vec<_>>()
        .join(" ");
    format!(
        "len={} preview_len={} hex=[{}] truncated={}",
        bytes.len(),
        preview.len(),
        hex,
        bytes.len() > preview.len()
    )
}

pub fn trace_bytes(stage: &str, bytes: &[u8]) {
    if enabled() {
        log::info!("INPUT_TRACE {} {}", stage, format_bytes(bytes));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn trace_requires_explicit_opt_in() {
        for v in [None, Some(""), Some("0"), Some("false")] {
            assert!(!env_value_enabled(v));
        }
        for v in ["1", "true", "YES", "on"] {
            assert!(env_value_enabled(Some(v)));
        }
    }
    #[test]
    fn formats_csi_without_control_characters() {
        assert!(format_bytes(b"\x1b[46;5u").contains("hex=[1b 5b 34 36 3b 35 75]"));
        assert!(!format_bytes(b"\r\n\x1b").chars().any(char::is_control));
    }
    #[test]
    fn preview_is_bounded() {
        let trace = format_bytes(&[0; 10000]);
        assert!(trace.contains("len=10000 preview_len=128"));
        assert!(trace.contains("truncated=true"));
        assert!(trace.len() < 500);
    }
}
