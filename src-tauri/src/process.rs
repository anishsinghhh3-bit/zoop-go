use std::{
    io::Read,
    process::{Child, ExitStatus},
    thread,
    time::Duration,
};

const POLL_INTERVAL: Duration = Duration::from_millis(100);
const DIAGNOSTIC_LIMIT: usize = 8 * 1024;

pub(super) fn wait_for_child(
    child: &mut Child,
    mut deadline: impl FnMut() -> Option<&'static str>,
) -> Result<ExitStatus, String> {
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Ok(status),
            Ok(None) => {}
            Err(error) => {
                terminate(child);
                return Err(format!("Could not determine the encoder status. {error}"));
            }
        }
        if let Some(message) = deadline() {
            terminate(child);
            return Err(message.into());
        }
        thread::sleep(POLL_INTERVAL);
    }
}

fn terminate(child: &mut Child) {
    if child.kill().is_ok() {
        let _ = child.wait();
    } else {
        // The process may have exited since the previous try_wait.
        let _ = child.try_wait();
    }
}

pub(super) fn read_diagnostic(mut reader: impl Read) -> String {
    let mut retained = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        match reader.read(&mut chunk) {
            Ok(0) => break,
            Ok(count) => {
                let keep = count.min(DIAGNOSTIC_LIMIT - retained.len());
                retained.extend_from_slice(&chunk[..keep]);
            }
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => break,
        }
    }
    String::from_utf8_lossy(&retained).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        io,
        process::{Command, Stdio},
        time::Instant,
    };

    const SLEEP_ENV: &str = "ZOOP_GO_TEST_SLOW_SUBPROCESS";

    #[test]
    fn sleeping_child_fixture() {
        if std::env::var_os(SLEEP_ENV).is_some() {
            thread::sleep(Duration::from_secs(60));
        }
    }

    #[test]
    fn deadline_reaps_unresponsive_child() {
        let suite = module_path!().split_once("::").unwrap().1;
        let test = format!("{suite}::sleeping_child_fixture");
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", &test])
            .env(SLEEP_ENV, "1")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_millis(250);
        let result = wait_for_child(&mut child, || {
            (Instant::now() >= deadline).then_some("encoder stalled")
        });
        assert_eq!(result.unwrap_err(), "encoder stalled");
        assert!(child.try_wait().unwrap().is_some());
    }

    #[test]
    fn stderr_reader_retains_a_prefix_and_drains_the_rest() {
        let mut input = io::repeat(b'x').take(30_000);
        let diagnostic = read_diagnostic(&mut input);
        assert_eq!(diagnostic.len(), DIAGNOSTIC_LIMIT);
        assert_eq!(input.limit(), 0);
    }
}
