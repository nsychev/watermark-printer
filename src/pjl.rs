const PJL_MAGIC: &[u8] = b"\x1B%-12345X";

#[derive(Debug)]
pub enum PjlError {
    NotPjlFile,
    UnsupportedPjlFile,
}

pub fn is_pjl(data: &[u8]) -> bool {
    data.starts_with(PJL_MAGIC)
}

pub fn extract_content(data: &[u8]) -> Result<Vec<u8>, PjlError> {
    if !is_pjl(data) {
        return Err(PjlError::NotPjlFile);
    }

    // Skip the initial PJL magic sequence
    let mut content = Vec::new();

    let mut found_start = false;
    for chunk in data.split(|&b| b == b'\n') {
        if !found_start {
            if chunk.starts_with(b"@PJL ENTER LANGUAGE = PDF") {
                found_start = true;
            }
            continue;
        }

        // Check for end sequence within the chunk
        if let Some(pos) = chunk.windows(PJL_MAGIC.len()).position(|window| window == PJL_MAGIC) {
            content.extend_from_slice(&chunk[..pos]);
            break;
        }

        content.extend_from_slice(chunk);
        content.push(b'\n');
    }

    if !found_start {
        return Err(PjlError::UnsupportedPjlFile);
    }

    Ok(content)
}
