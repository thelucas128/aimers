use serde_json::Value;
use std::time::Duration;

pub(crate) const ASSETS_PATH: &str = "/assets/user-auth/v1/assets";
pub(crate) const OPERATIONS_PATH: &str = "/assets/user-auth/v1/operations";

pub(crate) fn multipart(
    request: &Value,
    filename: &str,
    content_type: &str,
    bytes: &[u8],
) -> (String, Vec<u8>) {
    let mut boundary = "----rustudio-asset-upload-7c1f".to_owned();
    while bytes
        .windows(boundary.len())
        .any(|part| part == boundary.as_bytes())
    {
        boundary.push('x');
    }
    let mut body = format!(
        "--{boundary}\r\nContent-Disposition: form-data; name=\"request\"\r\nContent-Type: application/json\r\n\r\n{request}\r\n--{boundary}\r\nContent-Disposition: form-data; name=\"fileContent\"; filename=\"{filename}\"\r\nContent-Type: {content_type}\r\n\r\n"
    )
    .into_bytes();
    body.extend_from_slice(bytes);
    body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
    (format!("multipart/form-data; boundary={boundary}"), body)
}

pub(crate) fn wait_for_operation<E>(
    initial: &str,
    operations_url: &str,
    mut fetch: impl FnMut(&str) -> Result<String, E>,
    error: impl Fn(String) -> E,
) -> Result<Value, E> {
    let parse = |body: &str| {
        serde_json::from_str::<Value>(body)
            .map_err(|e| error(format!("Invalid upload operation reply: {e}")))
    };
    let mut operation = parse(initial)?;
    let mut poll_url = None;
    for attempt in 0..=60 {
        if let Some(failure) = operation.get("error").filter(|v| !v.is_null()) {
            return Err(error(format!("Upload operation failed: {failure}")));
        }
        if operation.get("done").and_then(Value::as_bool) == Some(true) {
            return Ok(operation);
        }
        if attempt == 60 {
            break;
        }
        if poll_url.is_none() {
            let path = operation
                .get("path")
                .or_else(|| operation.get("operationId"))
                .and_then(Value::as_str)
                .ok_or_else(|| error("Upload reply is missing its operation ID".into()))?;
            let absolute_prefix = format!("{operations_url}/");
            let route_prefix = format!("{OPERATIONS_PATH}/");
            let id = path
                .strip_prefix("operations/")
                .or_else(|| path.strip_prefix(&route_prefix))
                .or_else(|| path.strip_prefix(&absolute_prefix))
                .unwrap_or(path);
            // Only a single opaque ID can be appended to the configured endpoint.
            if id.is_empty()
                || !id
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
            {
                return Err(error(
                    "Upload reply contains an invalid operation path".into(),
                ));
            }
            poll_url = Some(format!("{operations_url}/{id}"));
        }
        std::thread::sleep(Duration::from_secs(1));
        operation = parse(&fetch(poll_url.as_deref().unwrap())?)?;
    }
    Err(error(
        "Upload operation did not finish after 60 polls".into(),
    ))
}
