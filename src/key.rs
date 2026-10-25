pub fn sanitize_camera_id(raw: &str) -> String {
    let trimmed = raw.trim();
    let candidate = if trimmed.is_empty() {
        "camera"
    } else {
        trimmed
    };
    candidate
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}
