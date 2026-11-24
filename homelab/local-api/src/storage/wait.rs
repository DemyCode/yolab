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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[should_panic(expected = "expected NotYet")]
    fn a_test_that_expects_waiting_fails_loudly_when_the_step_was_ready() {
        not_yet(Attempt::Ready(()));
    }
}
