#[derive(Debug, PartialEq, Eq)]
pub enum Attempt<T> {
    Ready(T),
    NotYet(String),
}

#[cfg(test)]
pub fn not_yet(a: Attempt<()>) -> String {
    match a {
        Attempt::NotYet(why) => why,
        Attempt::Ready(()) => panic!("expected NotYet"),
    }
}
