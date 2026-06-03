use std::env;

const INPUT_TRACE_ENV: &str = "ZELLIJ_INPUT_TRACE";
const MAX_PREVIEW_BYTES: usize = 128;

pub fn enabled() -> bool {
    env_value_enabled(env::var(INPUT_TRACE_ENV).ok().as_deref())
}

pub fn env_value_enabled(value: Option<&str>) -> bool {
    value
        .map(|value| {
            matches!(
                value.to_ascii_lowercase().as_str(),
                "1" | "true" | "yes" | "on"
            )
        })
        .unwrap_or(false)
}

pub fn format_bytes(bytes: &[u8]) -> String {
    let truncated = bytes.len() > MAX_PREVIEW_BYTES;
    let preview_len = bytes.len().min(MAX_PREVIEW_BYTES);
    let preview = &bytes[..preview_len];
    let hex = preview
        .iter()
        .map(|byte| format!("{:02x}", byte))
        .collect::<Vec<_>>()
        .join(" ");
    let ascii = preview
        .iter()
        .map(|byte| match *byte {
            0x1b => "ESC".to_owned(),
            b'\r' => "\\r".to_owned(),
            b'\n' => "\\n".to_owned(),
            b'\t' => "\\t".to_owned(),
            0x20..=0x7e => (*byte as char).to_string(),
            _ => format!("\\x{:02x}", byte),
        })
        .collect::<Vec<_>>()
        .join("");
    let suffix = if truncated { " truncated=true" } else { "" };
    format!(
        "len={} preview_len={} hex=[{}] ascii=\"{}\"{}",
        bytes.len(),
        preview_len,
        hex,
        ascii,
        suffix
    )
}

#[cfg(test)]
mod tests {
    use super::{env_value_enabled, format_bytes};

    #[test]
    fn input_trace_env_value_accepts_common_true_values() {
        assert!(env_value_enabled(Some("1")));
        assert!(env_value_enabled(Some("true")));
        assert!(env_value_enabled(Some("YES")));
        assert!(env_value_enabled(Some("on")));
        assert!(!env_value_enabled(Some("0")));
        assert!(!env_value_enabled(Some("false")));
        assert!(!env_value_enabled(None));
    }

    #[test]
    fn input_trace_formats_csi_u_bytes() {
        let formatted = format_bytes(b"\x1b[46;5u");
        assert!(formatted.contains("len=7"));
        assert!(formatted.contains("hex=[1b 5b 34 36 3b 35 75]"));
        assert!(formatted.contains("ascii=\"ESC[46;5u\""));
    }
}
