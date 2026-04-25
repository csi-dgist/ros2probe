use std::os::unix::process::CommandExt;
use std::process::{Child, Command, Stdio};

use anyhow::Context;

use crate::discovery::{EndpointEntry, Locator};

// FastDDS SHM locator kind (eProsima extension, defined in FastDDS as
// LOCATOR_KIND_SHM = 16). CycloneDDS iceoryx also advertises via this kind.
const LOCATOR_KIND_SHM: i32 = 16;

fn locator_is_shm(l: &Locator) -> bool {
    l.kind == LOCATOR_KIND_SHM
}

/// Returns true if the endpoint uses SHM transport, indicated by at least one
/// SHM-kind locator in its effective locator list.
///
/// We use `any` rather than `all` because FastDDS and CycloneDDS advertise SHM
/// locators alongside standard UDP locators (loopback + external interfaces).
/// `all` would reject any participant that has an external NIC, which is every
/// real machine — making shadow-sub spawning unreachable in practice.
pub fn endpoint_needs_shadow(endpoint: &EndpointEntry, participant_locators: &[Locator]) -> bool {
    let locators = if endpoint.unicast_locators.is_empty() {
        participant_locators
    } else {
        &endpoint.unicast_locators
    };
    locators.iter().any(locator_is_shm)
}

/// Manages a shadow subscriber subprocess for a single SHM-only topic.
///
/// On construction the subprocess is spawned inside an `unshare --mount`
/// namespace with /dev/shm masked and iceoryx RouDi sockets shadowed, so DDS
/// falls back to UDP over loopback. `ros2 topic hz` subscribes to the topic,
/// causing the publisher to discover the new subscriber via SEDP and begin
/// sending UDP unicast packets to 127.x.x.x, which ros2probe's eBPF filter
/// picks up on `lo` through the normal capture path.
///
/// Dropping `ShadowSubscriber` kills the subprocess.
pub struct ShadowSubscriber {
    child: Option<Child>,
    topic: String,
}

impl ShadowSubscriber {
    /// Spawn a shadow subscriber for `topic` (ROS 2 form, e.g. `"/chatter"`).
    pub fn spawn(topic: &str) -> anyhow::Result<Self> {
        super::write_shm_block_configs()?;

        let username = std::env::var("SUDO_USER")
            .ok()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "root".to_string());
        let ros_source = super::ros_setup_cmd();

        // topic contains only [A-Za-z0-9/_] so it is safe to embed inside the
        // single-quoted `su -c '...'` argument.
        let script = format!(
            r#"mount -t tmpfs tmpfs /dev/shm 2>/dev/null
for s in /tmp/roudi_uds_* /tmp/roudi_sld_* /tmp/iceoryx_*; do
    [ -e "$s" ] && mount --bind /dev/null "$s" 2>/dev/null
done
su -s /bin/sh {username} -c '{ros_source} . /run/ros2probe_env.sh && ros2 topic hz {topic} 2>/dev/null'
"#
        );

        let child = Command::new("unshare")
            .args(["--mount", "--", "sh", "-c", &script])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            // Put the child into its own process group so we can kill the
            // entire tree (unshare → sh → su → python) on drop by signaling
            // the negative PID. Without this, only `unshare` dies and the
            // grandchildren are reparented to init and keep running.
            .process_group(0)
            .spawn()
            .context("spawn shadow subscriber")?;

        Ok(Self { child: Some(child), topic: topic.to_string() })
    }

    pub fn topic(&self) -> &str {
        &self.topic
    }

    /// Converts an RTPS topic name (`"rt/chatter"`) to a ROS 2 topic name
    /// (`"/chatter"`). Returns `None` for non-user topics (services, etc.).
    pub fn ros2_topic(rtps_topic: &str) -> Option<String> {
        rtps_topic.strip_prefix("rt/").map(|s| format!("/{s}"))
    }
}

impl Drop for ShadowSubscriber {
    fn drop(&mut self) {
        // SIGTERM lets rclpy/FastDDS send the participant-remove announcement
        // so other nodes see us leave the ROS graph immediately — without it,
        // peers wait for SPDP lease expiry (~10s). Cleanup runs on a detached
        // thread so Drop itself returns in microseconds; the thread reaps the
        // child to avoid zombies and force-kills any descendant that ignores
        // SIGTERM. `su` starts a new session so SIGKILL on the root doesn't
        // cascade — we walk /proc to collect the tree before signaling.
        let Some(mut child) = self.child.take() else { return };
        let pid = child.id() as i32;
        let descendants = collect_descendants(pid);
        for p in &descendants {
            unsafe { libc::kill(*p, libc::SIGTERM); }
        }
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(500));
            for p in &descendants {
                unsafe { libc::kill(*p, libc::SIGKILL); }
            }
            let _ = child.wait();
        });
    }
}

fn collect_descendants(root: i32) -> Vec<i32> {
    let mut result = vec![root];
    let mut queue = vec![root];
    while let Some(pid) = queue.pop() {
        let task_dir = format!("/proc/{pid}/task");
        let Ok(entries) = std::fs::read_dir(&task_dir) else { continue };
        for entry in entries.flatten() {
            let path = entry.path().join("children");
            let Ok(content) = std::fs::read_to_string(&path) else { continue };
            for tok in content.split_whitespace() {
                if let Ok(cp) = tok.parse::<i32>() {
                    result.push(cp);
                    queue.push(cp);
                }
            }
        }
    }
    result
}
