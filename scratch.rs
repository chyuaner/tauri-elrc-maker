let mut range_start = 0;
let mut range_end = file_len - 1;
let mut has_range = false;

if let Some(range_header) = request.headers().iter()
    .find(|h| h.field.as_str().as_str().eq_ignore_ascii_case("range"))
{
    let range_str = range_header.value.as_str();
    if range_str.starts_with("bytes=") {
        let clean_range = &range_str["bytes=".len()..];
        let parts: Vec<&str> = clean_range.split('-').collect();
        if parts.len() == 2 {
            if parts[0].is_empty() && !parts[1].is_empty() {
                // Suffix byte range: "bytes=-500" means last 500 bytes
                if let Ok(suffix_len) = parts[1].parse::<usize>() {
                    range_start = file_len.saturating_sub(suffix_len);
                    range_end = file_len - 1;
                    has_range = true;
                }
            } else {
                if let Ok(start) = parts[0].parse::<usize>() {
                    range_start = start;
                    has_range = true;
                }
                if !parts[1].is_empty() {
                    if let Ok(end) = parts[1].parse::<usize>() {
                        range_end = end;
                        has_range = true;
                    }
                }
            }
        }
    }
}
