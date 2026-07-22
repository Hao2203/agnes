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

pub fn json_validator(v: &JsonValue) -> Result<(), String> {
    let s = v
        .as_str()
        .ok_or("JSON payload must be a string containing JSON")?;
    serde_json::from_str::<JsonValue>(s).map_err(|e| format!("not valid JSON: {e}"))?;
    Ok(())
}

pub fn unit_validator(v: &JsonValue) -> Result<(), String> {
    match v {
        JsonValue::Null => Ok(()),
        JsonValue::Object(m) if m.is_empty() => Ok(()),
        _ => Err("Unit must be null or {}".into()),
    }
}
