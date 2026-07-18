use serde_json::Value as JsonValue;

pub fn path_validator(v: &JsonValue) -> Result<(), String> {
    let s = v.as_str().ok_or("Path must be a JSON string")?;
    if s.is_empty() {
        return Err("Path is empty".into());
    }
    if s.contains('\0') {
        return Err("Path contains NUL byte".into());
    }
    Ok(())
}

pub fn utf8_validator(v: &JsonValue) -> Result<(), String> {
    let s = v.as_str().ok_or("expected JSON string")?;
    if std::str::from_utf8(s.as_bytes()).is_err() {
        return Err("value is not valid UTF-8".into());
    }
    Ok(())
}

pub fn json_validator(v: &JsonValue) -> Result<(), String> {
    let s = v
        .as_str()
        .ok_or("JSON payload must be a string containing JSON")?;
    serde_json::from_str::<JsonValue>(s).map_err(|e| format!("not valid JSON: {e}"))?;
    Ok(())
}

pub fn pdf_validator(v: &JsonValue) -> Result<(), String> {
    let arr = v
        .as_array()
        .ok_or("PDF must be a JSON array of byte integers")?;
    if arr.len() < 4 {
        return Err("PDF too short (missing %PDF header)".into());
    }
    let head: Vec<u8> = arr
        .iter()
        .take(4)
        .map(|n| n.as_u64().unwrap_or(0) as u8)
        .collect();
    if &head != b"%PDF" {
        return Err(format!("bad PDF magic: {head:?}"));
    }
    Ok(())
}

pub fn image_validator(v: &JsonValue) -> Result<(), String> {
    let arr = v
        .as_array()
        .ok_or("Image must be a JSON array of byte integers")?;
    if arr.len() < 4 {
        return Err("Image too short (missing magic bytes)".into());
    }
    let head: Vec<u8> = arr
        .iter()
        .take(8)
        .map(|n| n.as_u64().unwrap_or(0) as u8)
        .collect();
    // PNG, JPEG, GIF, WebP
    let magics: &[&[u8]] = &[b"\x89PNG", b"\xFF\xD8\xFF", b"GIF8", b"RIFF"];
    for m in magics {
        if head.starts_with(m) {
            return Ok(());
        }
    }
    Err(format!("no known image magic in head: {head:?}"))
}

pub fn unit_validator(v: &JsonValue) -> Result<(), String> {
    match v {
        JsonValue::Null => Ok(()),
        JsonValue::Object(m) if m.is_empty() => Ok(()),
        _ => Err("Unit must be null or {}".into()),
    }
}
