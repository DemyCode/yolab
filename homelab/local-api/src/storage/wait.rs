#[derive(Debug, PartialEq, Eq)]
pub enum Attempt<T> {
    Ready(T),
    NotYet(String),
}
