use std::{
    process::Command,
    time::{Duration, Instant},
};

use anyhow::Context;
use log::warn;

const DISCOVER_TIMEOUT: Duration = Duration::from_secs(8);

pub fn run_discovery() {
    if let Err(e) = discovery_round() {
        warn!("discovery: {e}");
    }
}

fn discovery_round() -> anyhow::Result<()> {
    let username = std::env::var("SUDO_USER")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "root".to_string());

    super::write_shm_block_configs()?;

    let ros_source = super::ros_setup_cmd();
    let script = format!(
        r#"mount -t tmpfs tmpfs /dev/shm 2>/dev/null
for s in /tmp/roudi_uds_* /tmp/roudi_sld_* /tmp/iceoryx_*; do
    [ -e "$s" ] && mount --bind /dev/null "$s" 2>/dev/null
done
su -s /bin/sh {username} -c '{ros_source} . /run/ros2probe_env.sh && ros2 node list --no-daemon 2>/dev/null'
"#
    );

    let mut child = Command::new("unshare")
        .args(["--mount", "--", "sh", "-c", &script])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .context("spawn unshare --mount")?;

    let deadline = Instant::now() + DISCOVER_TIMEOUT;
    loop {
        match child.try_wait().context("wait for discovery subprocess")? {
            Some(_) => break,
            None if Instant::now() >= deadline => {
                let _ = child.kill();
                break;
            }
            None => std::thread::sleep(Duration::from_millis(100)),
        }
    }

    Ok(())
}
