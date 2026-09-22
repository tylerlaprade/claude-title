use std::io::{self, Read};
use std::os::fd::AsRawFd;
use std::process::{Child, Command, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant};

pub fn nonblocking(stream: &impl AsRawFd) -> io::Result<()> {
    let fd = stream.as_raw_fd();
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags == -1 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

pub fn output(command: &mut Command) -> io::Result<Output> {
    output_with_timeout(command, Duration::from_secs(2))
}

fn output_with_timeout(command: &mut Command, timeout: Duration) -> io::Result<Output> {
    let mut child = ChildGuard(
        command
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?,
    );
    let mut stdout = child.0.stdout.take().unwrap();
    let mut stderr = child.0.stderr.take().unwrap();
    nonblocking(&stdout)?;
    nonblocking(&stderr)?;
    let mut out = Vec::new();
    let mut err = Vec::new();
    let deadline = Instant::now() + timeout;
    loop {
        let status = child.0.try_wait()?;
        let read_out = drain(&mut stdout, &mut out)?;
        let read_err = drain(&mut stderr, &mut err)?;
        if let Some(status) = status
            && !read_out
            && !read_err
        {
            return Ok(Output {
                status,
                stdout: out,
                stderr: err,
            });
        }
        if Instant::now() >= deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "process probe timed out",
            ));
        }
        if !read_out && !read_err {
            thread::sleep(Duration::from_millis(10));
        }
    }
}

fn drain(stream: &mut impl Read, output: &mut Vec<u8>) -> io::Result<bool> {
    let mut buffer = [0; 8192];
    match stream.read(&mut buffer) {
        Ok(size) => {
            output.extend_from_slice(&buffer[..size]);
            Ok(size != 0)
        }
        Err(error)
            if matches!(
                error.kind(),
                io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
            ) =>
        {
            Ok(false)
        }
        Err(error) => Err(error),
    }
}

struct ChildGuard(Child);

impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_probe_cannot_wait_forever() {
        let start = Instant::now();
        let error = output_with_timeout(Command::new("sleep").arg("30"), Duration::from_millis(50))
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert!(start.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn output_larger_than_a_pipe_buffer_does_not_deadlock() {
        let output = output(Command::new("head").args(["-c", "262144", "/dev/zero"])).unwrap();
        assert!(output.status.success());
        assert_eq!(output.stdout.len(), 262144);
    }

    #[test]
    fn a_probe_preserves_error_status_and_stderr() {
        let output =
            output(Command::new("sh").args(["-c", "printf diagnostic >&2; exit 2"])).unwrap();
        assert_eq!(output.status.code(), Some(2));
        assert_eq!(output.stderr, b"diagnostic");
    }
}
