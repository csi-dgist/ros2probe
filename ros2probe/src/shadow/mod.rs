pub mod discover;
pub mod sub;

use anyhow::Context;

const FASTDDS_NO_SHM: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<profiles xmlns="http://www.eprosima.com/XMLSchemas/fastRTPS_Profiles">
  <transport_descriptors>
    <transport_descriptor>
      <transport_id>udp4</transport_id>
      <type>UDPv4</type>
    </transport_descriptor>
  </transport_descriptors>
  <participant profile_name="default_xrce_participant" is_default_profile="true">
    <rtps>
      <userTransports><transport_id>udp4</transport_id></userTransports>
      <useBuiltinTransports>false</useBuiltinTransports>
    </rtps>
  </participant>
</profiles>"#;

const CYCLONEDDS_NO_SHM: &str =
    "<CycloneDDS><Domain><SharedMemory><Enable>false</Enable></SharedMemory></Domain></CycloneDDS>";

/// Write FastDDS XML and env-source script to /run/ so both `discover` and
/// `sub` subprocesses can source the same UDP-only, no-SHM configuration.
/// /run/ is not remounted by `unshare --mount`, so files written here are
/// visible inside the new mount namespace.
///
/// ROS_DOMAIN_ID and RMW_IMPLEMENTATION are forwarded from the current
/// process so that shadow subprocesses join the same DDS domain and use the
/// same middleware as the nodes being probed. If ros2probe is started via
/// `sudo`, run it as `sudo -E rp ...` to preserve those variables.
fn write_shm_block_configs() -> anyhow::Result<()> {
    std::fs::write("/run/ros2probe_fastdds.xml", FASTDDS_NO_SHM)
        .context("write FastDDS profile")?;

    let domain_id = std::env::var("ROS_DOMAIN_ID").unwrap_or_else(|_| "0".to_string());
    let rmw_impl = std::env::var("RMW_IMPLEMENTATION").unwrap_or_default();
    let rmw_line = if rmw_impl.is_empty() {
        String::new()
    } else {
        format!("export RMW_IMPLEMENTATION={rmw_impl}\n")
    };

    std::fs::write(
        "/run/ros2probe_env.sh",
        format!(
            "export CYCLONEDDS_URI='{CYCLONEDDS_NO_SHM}'\n\
             export FASTRTPS_DEFAULT_PROFILES_FILE=/run/ros2probe_fastdds.xml\n\
             export ROS_DOMAIN_ID={domain_id}\n\
             {rmw_line}",
        ),
    )
    .context("write env file")?;
    Ok(())
}

fn ros_setup_cmd() -> String {
    for distro in ["humble", "iron", "jazzy", "rolling"] {
        let path = format!("/opt/ros/{distro}/setup.sh");
        if std::path::Path::new(&path).exists() {
            return format!(". {path} &&");
        }
    }
    String::new()
}
