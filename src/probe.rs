use std::collections::{HashMap, HashSet};
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

pub enum ShellProbe {
    /// The task's process tree holds a listening TCP socket: it serves until
    /// killed rather than running to completion, so it will never wake the
    /// session on its own.
    Serving,
    Running,
    /// The task's output file exists but no process holds it open anymore.
    Gone,
    /// The task's output file cannot be found, so nothing can be said.
    Unknown,
}

// A background shell's processes are found through the task output file, not
// the command text: Claude Code redirects the shell's stdout and stderr to
// <tasks root>/<project>/<session>/tasks/<task id>.output, and every process
// in the tree inherits those descriptors. Command text cannot identify the
// processes — the spawn wrapper rewrites quotes, ps escapes control
// characters, and one command's text can embed another's. The lookup is
// scoped to the session so another session's same-named task can never
// decide this one's verdict.
#[must_use]
pub fn tasks(session_id: &str, task_ids: &[String]) -> Vec<ShellProbe> {
    tasks_under(&tasks_root(), session_id, task_ids)
}

// One call probes every shell of a probe cycle: the process table is read
// once and all trees share one listening-socket query, so the cost per cycle
// stays flat as pending shells accumulate.
fn tasks_under(root: &Path, session_id: &str, task_ids: &[String]) -> Vec<ShellProbe> {
    enum Partial {
        Done(ShellProbe),
        Tree(HashSet<u32>),
    }
    let mut children = None;
    let partials: Vec<Partial> = task_ids
        .iter()
        .map(|task_id| {
            if !path_safe(session_id) || !path_safe(task_id) {
                return Partial::Done(ShellProbe::Unknown);
            }
            let outputs = task_outputs(root, session_id, task_id);
            if outputs.is_empty() {
                return Partial::Done(ShellProbe::Unknown);
            }
            let mut holders = HashSet::new();
            let mut lsof_ran = false;
            for output in &outputs {
                if let Some(pids) = file_holders(output) {
                    lsof_ran = true;
                    holders.extend(pids);
                }
            }
            if !lsof_ran {
                // A probe that could not run must not read as "task ended":
                // Gone is the one verdict that releases the waiting title.
                return Partial::Done(ShellProbe::Unknown);
            }
            if holders.is_empty() {
                return Partial::Done(ShellProbe::Gone);
            }
            let mut tree = holders.clone();
            let children = children.get_or_insert_with(child_map);
            for pid in holders {
                descend(children, pid, &mut tree);
            }
            Partial::Tree(tree)
        })
        .collect();
    let union = partials
        .iter()
        .filter_map(|partial| match partial {
            Partial::Tree(tree) => Some(tree),
            Partial::Done(_) => None,
        })
        .flatten()
        .copied()
        .collect();
    let listeners = listening_pids(&union);
    partials
        .into_iter()
        .map(|partial| match partial {
            Partial::Done(verdict) => verdict,
            Partial::Tree(tree) => {
                if tree.iter().any(|pid| listeners.contains(pid)) {
                    ShellProbe::Serving
                } else {
                    ShellProbe::Running
                }
            }
        })
        .collect()
}

fn path_safe(value: &str) -> bool {
    !value.is_empty()
        && value
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || matches!(character, '-' | '_'))
}

fn tasks_root() -> PathBuf {
    match env::var_os("CLAUDE_TITLE_TASKS_ROOT") {
        Some(root) => PathBuf::from(root),
        None => PathBuf::from("/tmp").join(format!("claude-{}", unsafe { libc::getuid() })),
    }
}

fn task_outputs(root: &Path, session_id: &str, task_id: &str) -> Vec<PathBuf> {
    let file_name = format!("{task_id}.output");
    let mut outputs = Vec::new();
    let Ok(projects) = fs::read_dir(root) else {
        return outputs;
    };
    for project in projects.flatten() {
        let candidate = project
            .path()
            .join(session_id)
            .join("tasks")
            .join(&file_name);
        if candidate.is_file() {
            outputs.push(candidate);
        }
    }
    outputs
}

fn file_holders(path: &Path) -> Option<Vec<u32>> {
    let path = path.to_str()?;
    let output = Command::new("lsof")
        .args(["-t", "--", path])
        .output()
        .ok()?;
    Some(
        String::from_utf8_lossy(&output.stdout)
            .lines()
            .filter_map(|line| line.trim().parse().ok())
            .collect(),
    )
}

fn child_map() -> HashMap<u32, Vec<u32>> {
    let mut children: HashMap<u32, Vec<u32>> = HashMap::new();
    let output = Command::new("ps").args(["-axo", "pid=,ppid="]).output();
    if let Ok(output) = output {
        for line in String::from_utf8_lossy(&output.stdout).lines() {
            let mut parts = line.split_whitespace();
            let pid = parts.next().and_then(|value| value.parse::<u32>().ok());
            let ppid = parts.next().and_then(|value| value.parse::<u32>().ok());
            if let (Some(pid), Some(ppid)) = (pid, ppid) {
                children.entry(ppid).or_default().push(pid);
            }
        }
    }
    children
}

fn descend(children: &HashMap<u32, Vec<u32>>, root: u32, into: &mut HashSet<u32>) {
    let mut queue = vec![root];
    while let Some(pid) = queue.pop() {
        if let Some(direct) = children.get(&pid) {
            for child in direct {
                if into.insert(*child) {
                    queue.push(*child);
                }
            }
        }
    }
}

fn listening_pids(pids: &HashSet<u32>) -> HashSet<u32> {
    if pids.is_empty() {
        return HashSet::new();
    }
    let list = pids
        .iter()
        .map(u32::to_string)
        .collect::<Vec<_>>()
        .join(",");
    let output = Command::new("lsof")
        .args(["-t", "-a", "-nP", "-iTCP", "-sTCP:LISTEN", "-p", &list])
        .output();
    let Ok(output) = output else {
        return HashSet::new();
    };
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| line.trim().parse().ok())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::File;
    use std::process::{Child, Stdio};

    struct ChildGuard(Child);

    impl Drop for ChildGuard {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    fn probe_one(root: &Path, session_id: &str, task_id: &str) -> ShellProbe {
        tasks_under(root, session_id, &[task_id.to_string()])
            .pop()
            .unwrap()
    }

    fn plant(root: &Path, task_id: &str) -> PathBuf {
        let tasks = root.join("project").join("session").join("tasks");
        fs::create_dir_all(&tasks).unwrap();
        let path = tasks.join(format!("{task_id}.output"));
        File::create(&path).unwrap();
        path
    }

    #[test]
    fn missing_output_file_is_unknown() {
        let root = tempfile::tempdir().unwrap();
        assert!(matches!(
            probe_one(root.path(), "session", "absent1"),
            ShellProbe::Unknown
        ));
    }

    #[test]
    fn suspicious_task_ids_are_unknown() {
        let root = tempfile::tempdir().unwrap();
        assert!(matches!(
            probe_one(root.path(), "session", "../escape"),
            ShellProbe::Unknown
        ));
        assert!(matches!(
            probe_one(root.path(), "session", ""),
            ShellProbe::Unknown
        ));
    }

    #[test]
    fn unheld_output_file_is_gone() {
        let root = tempfile::tempdir().unwrap();
        plant(root.path(), "finished1");
        assert!(matches!(
            probe_one(root.path(), "session", "finished1"),
            ShellProbe::Gone
        ));
    }

    #[test]
    fn another_sessions_same_named_task_is_not_consulted() {
        let root = tempfile::tempdir().unwrap();
        plant(root.path(), "shared1");
        let foreign = root.path().join("project").join("other").join("tasks");
        fs::create_dir_all(&foreign).unwrap();
        let foreign_output = foreign.join("shared1.output");
        let holder = ChildGuard(
            Command::new("sleep")
                .arg("30")
                .stdin(Stdio::null())
                .stdout(File::create(&foreign_output).unwrap())
                .stderr(Stdio::null())
                .spawn()
                .unwrap(),
        );
        // This session's file has no holders, so the verdict is Gone even
        // though another session's task with the same id is alive.
        let probed = probe_one(root.path(), "session", "shared1");
        drop(holder);
        assert!(matches!(probed, ShellProbe::Gone));
    }

    #[test]
    fn listening_descendant_of_the_holder_is_serving() {
        let root = tempfile::tempdir().unwrap();
        let path = plant(root.path(), "nested1");
        // The listener redirects both streams away, so only the sh wrapper
        // holds the output file and the socket is reachable through the
        // descendant walk alone. The trailing `:` keeps sh from replacing
        // itself with the python process.
        let holder = ChildGuard(
            Command::new("sh")
                .args([
                    "-c",
                    r#"python3 -c 'import socket,time; s=socket.socket(); s.bind(("127.0.0.1",0)); s.listen(1); time.sleep(30)' > /dev/null 2>&1; :"#,
                ])
                .stdin(Stdio::null())
                .stdout(File::create(&path).unwrap())
                .stderr(Stdio::null())
                .spawn()
                .unwrap(),
        );
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let mut verdict = probe_one(root.path(), "session", "nested1");
        while !matches!(verdict, ShellProbe::Serving) && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(100));
            verdict = probe_one(root.path(), "session", "nested1");
        }
        drop(holder);
        assert!(matches!(verdict, ShellProbe::Serving));
    }

    #[test]
    fn held_output_file_without_sockets_is_running() {
        let root = tempfile::tempdir().unwrap();
        let path = plant(root.path(), "busy1");
        let holder = ChildGuard(
            Command::new("sleep")
                .arg("30")
                .stdin(Stdio::null())
                .stdout(File::create(&path).unwrap())
                .stderr(Stdio::null())
                .spawn()
                .unwrap(),
        );
        let probed = probe_one(root.path(), "session", "busy1");
        drop(holder);
        assert!(matches!(probed, ShellProbe::Running));
    }
}
