use std::collections::{HashMap, HashSet};
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

pub enum ShellProbe {
    /// Runs until killed — a listening socket, or nothing but shells and a
    /// `tail` — so it will never wake the session.
    Endless,
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
    let mut table = None;
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
            let table = table.get_or_insert_with(ProcessTable::capture);
            for pid in holders {
                table.descend(pid, &mut tree);
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
                let listens = tree.iter().any(|pid| listeners.contains(pid));
                if listens || table.as_ref().is_some_and(|table| table.bare_tail(&tree)) {
                    ShellProbe::Endless
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

struct ProcessTable {
    children: HashMap<u32, Vec<u32>>,
    programs: HashMap<u32, String>,
}

impl ProcessTable {
    // `comm` is the executable, not the command line, so none of the quoting,
    // escaping, or substring hazards that rule out command-text matching
    // apply to it.
    fn capture() -> Self {
        let mut children: HashMap<u32, Vec<u32>> = HashMap::new();
        let mut programs = HashMap::new();
        let output = Command::new("/bin/ps")
            .args(["-axo", "pid=,ppid=,comm="])
            .output();
        if let Ok(output) = output {
            for line in String::from_utf8_lossy(&output.stdout).lines() {
                let mut parts = line.split_whitespace();
                let pid = parts.next().and_then(|value| value.parse::<u32>().ok());
                let ppid = parts.next().and_then(|value| value.parse::<u32>().ok());
                let (Some(pid), Some(ppid)) = (pid, ppid) else {
                    continue;
                };
                children.entry(ppid).or_default().push(pid);
                let comm = parts.collect::<Vec<_>>().join(" ");
                let program = comm
                    .rsplit('/')
                    .next()
                    .unwrap_or(&comm)
                    .trim_start_matches('-')
                    .to_string();
                programs.insert(pid, program);
            }
        }
        Self { children, programs }
    }

    fn descend(&self, root: u32, into: &mut HashSet<u32>) {
        let mut queue = vec![root];
        while let Some(pid) = queue.pop() {
            if let Some(direct) = self.children.get(&pid) {
                for child in direct {
                    if into.insert(*child) {
                        queue.push(*child);
                    }
                }
            }
        }
    }

    // A bare log tail never exits, so it must not hold the waiting title. But
    // `tail -f log | grep -m1 done` is awaited — the shell exits when grep
    // matches — so the tree must hold nothing beyond shells and tails.
    fn bare_tail(&self, tree: &HashSet<u32>) -> bool {
        let mut saw_tail = false;
        for pid in tree {
            match self.programs.get(pid).map(String::as_str) {
                Some("tail") => saw_tail = true,
                Some("sh" | "bash" | "zsh" | "dash" | "fish" | "ksh") => {}
                _ => return false,
            }
        }
        saw_tail
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
        let probed = probe_one(root.path(), "session", "shared1");
        drop(holder);
        assert!(matches!(probed, ShellProbe::Gone));
    }

    #[test]
    fn bare_tail_is_endless() {
        let root = tempfile::tempdir().unwrap();
        let log = root.path().join("app.log");
        File::create(&log).unwrap();
        let path = plant(root.path(), "tailed1");
        let holder = ChildGuard(
            Command::new("tail")
                .arg("-f")
                .arg(&log)
                .stdin(Stdio::null())
                .stdout(File::create(&path).unwrap())
                .stderr(Stdio::null())
                .spawn()
                .unwrap(),
        );
        let probed = probe_one(root.path(), "session", "tailed1");
        drop(holder);
        assert!(matches!(probed, ShellProbe::Endless));
    }

    #[test]
    fn tail_piped_into_a_matcher_is_awaited() {
        let root = tempfile::tempdir().unwrap();
        let log = root.path().join("app.log");
        File::create(&log).unwrap();
        let path = plant(root.path(), "tailwait1");
        let holder = ChildGuard(
            Command::new("sh")
                .args(["-c", &format!("tail -f {} | grep -m1 zzz", log.display())])
                .stdin(Stdio::null())
                .stdout(File::create(&path).unwrap())
                .stderr(Stdio::null())
                .spawn()
                .unwrap(),
        );
        std::thread::sleep(std::time::Duration::from_millis(300));
        let probed = probe_one(root.path(), "session", "tailwait1");
        drop(holder);
        assert!(matches!(probed, ShellProbe::Running));
    }

    #[test]
    fn listening_descendant_of_the_holder_is_endless() {
        let root = tempfile::tempdir().unwrap();
        let path = plant(root.path(), "nested1");
        // Both streams redirected away: the sh wrapper alone holds the file,
        // so the socket is reachable only through descent. The trailing `:`
        // keeps sh from exec-replacing itself with python.
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
        while !matches!(verdict, ShellProbe::Endless) && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(100));
            verdict = probe_one(root.path(), "session", "nested1");
        }
        drop(holder);
        assert!(matches!(verdict, ShellProbe::Endless));
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
