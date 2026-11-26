#[cfg(not(test))]
pub use crate::live::scratch_dir as dir;

#[cfg(test)]
thread_local! {
    static DIR: std::cell::RefCell<Option<std::path::PathBuf>> = const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
pub fn dir() -> std::path::PathBuf {
    DIR.with(|d| d.borrow().clone())
        .unwrap_or_else(std::env::temp_dir)
}

#[cfg(test)]
pub fn set(path: Option<std::path::PathBuf>) {
    DIR.with(|d| *d.borrow_mut() = path);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn scratch_files_go_to_the_system_temp_folder_unless_a_test_moves_them() {
        assert_eq!(dir(), std::env::temp_dir());
        set(Some(PathBuf::from("/nonexistent/scratch")));
        assert_eq!(dir(), PathBuf::from("/nonexistent/scratch"));
        set(None);
        assert_eq!(dir(), std::env::temp_dir());
    }
}
