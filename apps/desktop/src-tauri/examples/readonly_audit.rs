use std::io::Write;
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let output = std::env::args_os()
        .nth(1)
        .ok_or("expected an output JSON path")?;
    let result = pinkbin_desktop_lib::readonly_audit().map_err(std::io::Error::other)?;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(output)?;
    file.write_all(serde_json::to_string_pretty(&result)?.as_bytes())?;
    Ok(())
}
