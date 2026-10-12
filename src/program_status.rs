//! OSC 7501 reports, owned exclusively by the interactive terminal surface.
use std::io::Write;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Status {
    Idle,
    Working,
    Done,
    Error,
    Permission,
    Question,
    Auth,
}

impl Status {
    fn pairs(self) -> &'static str {
        match self {
            Self::Idle => "state=idle",
            Self::Working => "state=working",
            Self::Done => "state=done",
            Self::Error => "state=error",
            Self::Permission => "state=blocked:kind=permission",
            Self::Question => "state=blocked:kind=question",
            Self::Auth => "state=blocked:kind=auth",
        }
    }
}

pub(crate) struct Reporter {
    writer: Box<dyn Write + Send>,
    last: Option<Status>,
}

impl Reporter {
    pub(crate) fn new(writer: impl Write + Send + 'static) -> Self {
        Self {
            writer: Box::new(writer),
            last: None,
        }
    }

    pub(crate) fn report(&mut self, status: Status) {
        if self.last == Some(status) {
            return;
        }
        // Status is advisory: a terminal write failure must not stop the agent.
        if write!(self.writer, "\x1b]7501;{}:app=sigit\x1b\\", status.pairs())
            .and_then(|()| self.writer.flush())
            .is_ok()
        {
            self.last = Some(status);
        }
    }
}

impl Drop for Reporter {
    fn drop(&mut self) {
        // Records survive the alternate screen. Remove ours on every exit path,
        // including unwinding. The TUI uses the protocol's root record.
        if self.last.is_some() {
            let _ = self.writer.write_all(b"\x1b]7501;state=clear\x1b\\");
            let _ = self.writer.flush();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    #[derive(Clone, Default)]
    struct Output(Arc<Mutex<Vec<u8>>>);
    impl Write for Output {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn lifecycle_reports_are_framed_deduplicated_and_cleared() {
        let output = Output::default();
        {
            let mut reporter = Reporter::new(output.clone());
            for status in [
                Status::Idle,
                Status::Working,
                Status::Working,
                Status::Permission,
                Status::Working,
                Status::Question,
                Status::Auth,
                Status::Done,
                Status::Error,
                Status::Idle,
            ] {
                reporter.report(status);
            }
        }
        let actual = String::from_utf8(output.0.lock().unwrap().clone()).unwrap();
        let expected = [
            "state=idle",
            "state=working",
            "state=blocked:kind=permission",
            "state=working",
            "state=blocked:kind=question",
            "state=blocked:kind=auth",
            "state=done",
            "state=error",
            "state=idle",
        ]
        .map(|pairs| format!("\x1b]7501;{pairs}:app=sigit\x1b\\"))
        .join("")
            + "\x1b]7501;state=clear\x1b\\";
        assert_eq!(actual, expected);
    }

    #[test]
    fn unwinding_clears_the_terminal_record() {
        let output = Output::default();
        let captured = output.clone();
        let _ = std::panic::catch_unwind(move || {
            let mut reporter = Reporter::new(captured);
            reporter.report(Status::Working);
            panic!("terminal loop failed");
        });
        assert!(
            output
                .0
                .lock()
                .unwrap()
                .ends_with(b"\x1b]7501;state=clear\x1b\\")
        );
    }

    #[test]
    fn unused_reporter_emits_nothing() {
        let output = Output::default();
        drop(Reporter::new(output.clone()));
        assert!(output.0.lock().unwrap().is_empty());
    }
}
