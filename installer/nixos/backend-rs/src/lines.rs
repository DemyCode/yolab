use crate::app::AppEvent;
use crate::seam::Events;

pub async fn forward_lines<R: tokio::io::AsyncBufRead + Unpin>(
    mut lines: tokio::io::Lines<R>,
    tx: &Events,
) {
    loop {
        match lines.next_line().await {
            Ok(Some(line)) => {
                let _ = tx.send(AppEvent::Log(line));
            }
            Ok(None) => return,
            Err(e) => {
                let _ = tx.send(AppEvent::Log(format!("(could not read more output: {e})")));
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncBufReadExt, BufReader};
    use tokio::sync::mpsc;

    fn logged(rx: &mut mpsc::UnboundedReceiver<AppEvent>) -> Vec<AppEvent> {
        let mut out = vec![];
        while let Ok(ev) = rx.try_recv() {
            out.push(ev);
        }
        out
    }

    fn log_lines(lines: &[&str]) -> Vec<AppEvent> {
        lines.iter().map(|l| AppEvent::Log(l.to_string())).collect()
    }

    #[tokio::test]
    async fn every_line_reaches_the_log_in_order() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let input: &[u8] = b"first\nsecond\nthird";
        forward_lines(BufReader::new(input).lines(), &tx).await;
        assert_eq!(logged(&mut rx), log_lines(&["first", "second", "third"]));
    }

    #[tokio::test]
    async fn output_that_is_not_text_ends_the_log_with_a_note_instead_of_silence() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let input: &[u8] = b"good\n\xff\xfe\nnever read\n";
        forward_lines(BufReader::new(input).lines(), &tx).await;
        let evs = logged(&mut rx);
        assert_eq!(evs.len(), 2);
        assert_eq!(evs[0], AppEvent::Log("good".into()));
        assert!(
            format!("{:?}", evs[1]).starts_with("Log(\"(could not read more output: "),
            "{evs:?}"
        );
    }

    #[tokio::test]
    async fn a_closed_log_does_not_stop_the_command_being_read() {
        let (tx, rx) = mpsc::unbounded_channel();
        drop(rx);
        let input: &[u8] = b"a\nb\n";
        forward_lines(BufReader::new(input).lines(), &tx).await;
    }
}
