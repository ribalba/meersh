use std::path::PathBuf;

/// Expands a leading `~` to the home directory.
pub fn expand_tilde(path: &str) -> String {
    match path.strip_prefix('~') {
        Some(rest) if rest.is_empty() || rest.starts_with('/') => {
            dirs::home_dir().map_or_else(|| path.to_string(), |home| format!("{}{rest}", home.display()))
        }
        _ => path.to_string(),
    }
}

pub fn expand_path(path: &str) -> PathBuf {
    PathBuf::from(expand_tilde(path))
}

pub fn is_glob(word: &str) -> bool {
    word.contains(['*', '?', '['])
}

/// Expands a glob like bash does. A pattern without matches stays as it is.
pub fn expand_glob(word: &str) -> Vec<String> {
    let word = expand_tilde(word);
    if !is_glob(&word) {
        return vec![word];
    }
    let matches: Vec<String> = glob::glob(&word)
        .map(|paths| paths.filter_map(|p| p.ok()).map(|p| p.display().to_string()).collect())
        .unwrap_or_default();
    if matches.is_empty() { vec![word] } else { matches }
}
