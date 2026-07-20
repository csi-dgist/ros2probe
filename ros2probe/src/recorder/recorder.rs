use std::{
    borrow::Cow,
    collections::{BTreeMap, HashMap},
    env,
    ffi::CString,
    fs::{self, File},
    io::BufWriter,
    path::{Path, PathBuf},
};

use anyhow::{Context, bail};
use bytes::Bytes;
use libc;
use mcap::{
    Compression, WriteOptions, Writer,
    records::{MessageHeader, Metadata},
};
use serde::Serialize;

use crate::runtime::CompressionConfig;

#[derive(Clone, Debug, Eq, PartialEq, Hash)]
struct ChannelKey {
    topic: String,
    schema_name: String,
    qos_profile: String,
}

#[derive(Clone, Debug)]
pub(crate) struct RecordMessage {
    pub channel_id: u16,
    pub sequence: u32,
    pub log_time: u64,
    pub publish_time: u64,
    pub payload: Bytes,
}

#[derive(Clone, Debug)]
struct LastChannel {
    topic: String,
    schema_name: String,
    qos_profile: String,
    channel_id: u16,
}

#[derive(Clone, Debug)]
struct TopicSummary {
    topic: String,
    schema_name: String,
    message_count: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Rosbag2MetadataVersion {
    V5,
    V9,
}

impl Rosbag2MetadataVersion {
    const fn as_u32(self) -> u32 {
        match self {
            Self::V5 => 5,
            Self::V9 => 9,
        }
    }
}

pub struct Recorder {
    path: PathBuf,
    writer: Writer<BufWriter<File>>,
    schema_ids: HashMap<String, u16>,
    channel_ids: HashMap<ChannelKey, u16>,
    channel_summaries: BTreeMap<u16, TopicSummary>,
    last_channel: Option<LastChannel>,
    ament_prefixes: Vec<PathBuf>,
    message_count: u64,
    first_message_time: Option<u64>,
    last_message_time: Option<u64>,
    ros_distro: String,
}

impl Recorder {
    pub(crate) fn create(
        path: impl AsRef<Path>,
        compression: CompressionConfig,
    ) -> anyhow::Result<Self> {
        let path = path.as_ref().to_path_buf();
        if let Some(parent) = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            fs::create_dir_all(parent)
                .with_context(|| format!("create output directory {}", parent.display()))?;
        }
        let file =
            File::create(&path).with_context(|| format!("create MCAP file {}", path.display()))?;
        let writer = WriteOptions::new()
            .profile("ros2")
            .library("ros2probe")
            .compression(match compression {
                CompressionConfig::None => None,
                CompressionConfig::Zstd => Some(Compression::Zstd),
                CompressionConfig::Lz4 => Some(Compression::Lz4),
            })
            .create(BufWriter::new(file))
            .context("create MCAP writer")?;

        Ok(Self {
            path,
            writer,
            schema_ids: HashMap::new(),
            channel_ids: HashMap::new(),
            channel_summaries: BTreeMap::new(),
            last_channel: None,
            ament_prefixes: ament_prefixes(),
            message_count: 0,
            first_message_time: None,
            last_message_time: None,
            ros_distro: env::var("ROS_DISTRO").unwrap_or_else(|_| String::from("unknown")),
        })
    }

    pub(crate) fn ensure_channel(
        &mut self,
        topic_name: &str,
        schema_name: &str,
        qos_profile: &str,
    ) -> anyhow::Result<u16> {
        let normalized_schema_name = normalize_schema_name(schema_name);
        self.channel_id_for(topic_name, normalized_schema_name.as_ref(), qos_profile)
    }

    pub(crate) fn write_record_message(&mut self, message: &RecordMessage) -> anyhow::Result<()> {
        self.writer
            .write_to_known_channel(
                &MessageHeader {
                    channel_id: message.channel_id,
                    sequence: message.sequence,
                    log_time: message.log_time,
                    publish_time: message.publish_time,
                },
                &message.payload,
            )
            .context("write MCAP message")?;
        self.record_message_summary(message);
        Ok(())
    }

    pub(crate) fn finish(mut self) -> anyhow::Result<PathBuf> {
        let metadata = self
            .rosbag2_metadata()
            .context("serialize rosbag2 MCAP metadata")?;
        self.writer
            .write_metadata(&metadata)
            .context("write rosbag2 MCAP metadata")?;
        self.writer.finish().context("finish MCAP writer")?;
        restore_ownership(&self.path);
        Ok(self.path)
    }

    fn channel_id_for(
        &mut self,
        topic_name: &str,
        normalized_schema_name: &str,
        qos_profile: &str,
    ) -> anyhow::Result<u16> {
        if let Some(last_channel) = &self.last_channel {
            if last_channel.topic == topic_name
                && last_channel.schema_name == normalized_schema_name
                && last_channel.qos_profile == qos_profile
            {
                return Ok(last_channel.channel_id);
            }
        }

        let schema_id =
            if let Some(schema_id) = self.schema_ids.get(normalized_schema_name).copied() {
                schema_id
            } else {
                let resolved = resolve_schema(normalized_schema_name, &self.ament_prefixes)
                    .with_context(|| format!("resolve schema {normalized_schema_name}"))?;
                let schema_id = self
                    .writer
                    .add_schema(
                        &resolved.name,
                        resolved.encoding.as_str(),
                        resolved.text.as_bytes(),
                    )
                    .context("add MCAP schema")?;
                self.schema_ids
                    .insert(normalized_schema_name.to_string(), schema_id);
                schema_id
            };

        let channel_key = ChannelKey {
            topic: topic_name.to_string(),
            schema_name: normalized_schema_name.to_string(),
            qos_profile: qos_profile.to_string(),
        };
        let channel_id = if let Some(channel_id) = self.channel_ids.get(&channel_key).copied() {
            channel_id
        } else {
            let mut channel_metadata = BTreeMap::new();
            channel_metadata.insert(
                String::from("offered_qos_profiles"),
                qos_profile.to_string(),
            );
            let channel_id = self
                .writer
                .add_channel(schema_id, topic_name, "cdr", &channel_metadata)
                .context("add MCAP channel")?;
            self.channel_ids.insert(channel_key, channel_id);
            self.channel_summaries.insert(
                channel_id,
                TopicSummary {
                    topic: topic_name.to_string(),
                    schema_name: normalized_schema_name.to_string(),
                    message_count: 0,
                },
            );
            channel_id
        };

        self.last_channel = Some(LastChannel {
            topic: topic_name.to_string(),
            schema_name: normalized_schema_name.to_string(),
            qos_profile: qos_profile.to_string(),
            channel_id,
        });
        Ok(channel_id)
    }

    fn record_message_summary(&mut self, message: &RecordMessage) {
        self.message_count = self.message_count.saturating_add(1);
        if let Some(summary) = self.channel_summaries.get_mut(&message.channel_id) {
            summary.message_count = summary.message_count.saturating_add(1);
        }
        self.first_message_time = Some(
            self.first_message_time
                .map_or(message.log_time, |time| time.min(message.log_time)),
        );
        self.last_message_time = Some(
            self.last_message_time
                .map_or(message.log_time, |time| time.max(message.log_time)),
        );
    }

    fn rosbag2_metadata(&self) -> anyhow::Result<Metadata> {
        let mut metadata = BTreeMap::new();
        metadata.insert(
            String::from("serialized_metadata"),
            rosbag2_serialized_metadata(
                &self.path,
                rosbag2_metadata_version(&self.ros_distro),
                self.message_count,
                self.first_message_time.unwrap_or(0),
                self.duration_nanos(),
                self.channel_summaries.values(),
                &self.ros_distro,
            )?,
        );
        Ok(Metadata {
            name: String::from("rosbag2"),
            metadata,
        })
    }

    fn duration_nanos(&self) -> u64 {
        match (self.first_message_time, self.last_message_time) {
            (Some(first), Some(last)) => last.saturating_sub(first),
            _ => 0,
        }
    }
}

#[derive(Serialize)]
struct Rosbag2SerializedMetadata<'a> {
    version: u32,
    storage_identifier: &'static str,
    duration: Rosbag2Duration,
    starting_time: Rosbag2Timestamp,
    message_count: u64,
    topics_with_message_count: Vec<Rosbag2TopicInformation<'a>>,
    compression_format: &'static str,
    compression_mode: &'static str,
    relative_file_paths: Vec<String>,
    files: Vec<Rosbag2FileInformation>,
    #[serde(skip_serializing_if = "Option::is_none")]
    custom_data: Option<BTreeMap<String, String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    ros_distro: Option<&'a str>,
}

#[derive(Serialize)]
struct Rosbag2Duration {
    nanoseconds: u64,
}

#[derive(Serialize)]
struct Rosbag2Timestamp {
    nanoseconds_since_epoch: u64,
}

#[derive(Serialize)]
struct Rosbag2TopicInformation<'a> {
    topic_metadata: Rosbag2TopicMetadata<'a>,
    message_count: u64,
}

#[derive(Serialize)]
struct Rosbag2TopicMetadata<'a> {
    name: &'a str,
    #[serde(rename = "type")]
    type_name: &'a str,
    serialization_format: &'static str,
    offered_qos_profiles: serde_yaml::Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    type_description_hash: Option<&'static str>,
}

#[derive(Serialize)]
struct Rosbag2FileInformation {
    path: String,
    starting_time: Rosbag2Timestamp,
    duration: Rosbag2Duration,
    message_count: u64,
}

fn rosbag2_serialized_metadata<'a>(
    path: &Path,
    version: Rosbag2MetadataVersion,
    message_count: u64,
    starting_time_nanos: u64,
    duration_nanos: u64,
    topics: impl IntoIterator<Item = &'a TopicSummary>,
    ros_distro: &'a str,
) -> anyhow::Result<String> {
    let relative_path = path
        .file_name()
        .and_then(|name| name.to_str())
        .map(str::to_string)
        .unwrap_or_else(|| path.display().to_string());
    let topic_infos = topics
        .into_iter()
        .map(|topic| Rosbag2TopicInformation {
            topic_metadata: Rosbag2TopicMetadata {
                name: &topic.topic,
                type_name: &topic.schema_name,
                serialization_format: "cdr",
                offered_qos_profiles: match version {
                    Rosbag2MetadataVersion::V5 => serde_yaml::Value::String(String::new()),
                    Rosbag2MetadataVersion::V9 => serde_yaml::Value::Sequence(Vec::new()),
                },
                type_description_hash: match version {
                    Rosbag2MetadataVersion::V5 => None,
                    Rosbag2MetadataVersion::V9 => Some(""),
                },
            },
            message_count: topic.message_count,
        })
        .collect::<Vec<_>>();

    let metadata = Rosbag2SerializedMetadata {
        version: version.as_u32(),
        storage_identifier: "mcap",
        duration: Rosbag2Duration {
            nanoseconds: duration_nanos,
        },
        starting_time: Rosbag2Timestamp {
            nanoseconds_since_epoch: starting_time_nanos,
        },
        message_count,
        topics_with_message_count: topic_infos,
        compression_format: "",
        compression_mode: "",
        relative_file_paths: vec![relative_path.clone()],
        files: vec![Rosbag2FileInformation {
            path: relative_path,
            starting_time: Rosbag2Timestamp {
                nanoseconds_since_epoch: starting_time_nanos,
            },
            duration: Rosbag2Duration {
                nanoseconds: duration_nanos,
            },
            message_count,
        }],
        custom_data: match version {
            Rosbag2MetadataVersion::V5 => None,
            Rosbag2MetadataVersion::V9 => Some(BTreeMap::new()),
        },
        ros_distro: match version {
            Rosbag2MetadataVersion::V5 => None,
            Rosbag2MetadataVersion::V9 => Some(ros_distro),
        },
    };

    serde_yaml::to_string(&metadata).context("serialize rosbag2 BagMetadata YAML")
}

fn rosbag2_metadata_version(ros_distro: &str) -> Rosbag2MetadataVersion {
    if let Ok(version) = env::var("ROS2PROBE_ROSBAG2_METADATA_VERSION") {
        match version.as_str() {
            "5" => return Rosbag2MetadataVersion::V5,
            "9" => return Rosbag2MetadataVersion::V9,
            _ => log::warn!(
                "ignoring unsupported ROS2PROBE_ROSBAG2_METADATA_VERSION={version}; expected 5 or 9"
            ),
        }
    }

    match ros_distro {
        "jazzy" | "kilted" | "rolling" => Rosbag2MetadataVersion::V9,
        _ => Rosbag2MetadataVersion::V5,
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ResolvedSchema {
    name: String,
    encoding: SchemaEncoding,
    text: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SchemaEncoding {
    Ros2Msg,
    Ros2Idl,
}

impl SchemaEncoding {
    fn as_str(self) -> &'static str {
        match self {
            SchemaEncoding::Ros2Msg => "ros2msg",
            SchemaEncoding::Ros2Idl => "ros2idl",
        }
    }

    fn extension(self) -> &'static str {
        match self {
            SchemaEncoding::Ros2Msg => "msg",
            SchemaEncoding::Ros2Idl => "idl",
        }
    }
}

fn resolve_schema(name: &str, prefixes: &[PathBuf]) -> anyhow::Result<ResolvedSchema> {
    let type_name = ParsedTypeName::parse(name)?;

    for prefix in prefixes {
        for encoding in [SchemaEncoding::Ros2Msg, SchemaEncoding::Ros2Idl] {
            let candidate = schema_candidate_path(prefix, &type_name, encoding);
            if !candidate.is_file() {
                continue;
            }

            let text = fs::read_to_string(&candidate)
                .with_context(|| format!("read schema file {}", candidate.display()))?;
            return Ok(ResolvedSchema {
                name: type_name.normalized_name(),
                encoding,
                text,
            });
        }
    }

    bail!(
        "unable to resolve schema {name} from AMENT_PREFIX_PATH; looked for {}/{}.[msg|idl] under each prefix",
        type_name.package,
        type_name.relative_stem()
    )
}

fn ament_prefixes() -> Vec<PathBuf> {
    let mut prefixes = env::var_os("AMENT_PREFIX_PATH")
        .map(|value| env::split_paths(&value).collect::<Vec<_>>())
        .unwrap_or_default();

    let opt_ros = PathBuf::from("/opt/ros");
    if let Ok(entries) = fs::read_dir(&opt_ros) {
        let mut discovered = entries
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| path.is_dir())
            .collect::<Vec<_>>();
        discovered.sort();
        prefixes.extend(discovered);
    }

    prefixes.sort();
    prefixes.dedup();
    prefixes
}

fn schema_candidate_path(
    prefix: &Path,
    type_name: &ParsedTypeName<'_>,
    encoding: SchemaEncoding,
) -> PathBuf {
    prefix
        .join("share")
        .join(type_name.package)
        .join(type_name.kind)
        .join(format!("{}.{}", type_name.type_name, encoding.extension()))
}

struct ParsedTypeName<'a> {
    package: &'a str,
    kind: &'a str,
    type_name: &'a str,
}

impl<'a> ParsedTypeName<'a> {
    fn parse(name: &'a str) -> anyhow::Result<Self> {
        if name.contains('/') {
            let mut parts = name.split('/');
            let package = parts.next().context("missing package in schema name")?;
            let kind = parts.next().context("missing kind in schema name")?;
            let type_name = parts.next().context("missing type name in schema name")?;

            if parts.next().is_some() {
                bail!("unexpected extra path segments in schema name {name}");
            }
            if !matches!(kind, "msg" | "srv" | "action") {
                bail!("unsupported schema kind {kind} in {name}");
            }

            return Ok(Self {
                package,
                kind,
                type_name,
            });
        }

        if let Some((package, kind, type_name)) = parse_dds_type_name(name) {
            return Ok(Self {
                package,
                kind,
                type_name,
            });
        }

        bail!("unsupported schema name format {name}")
    }

    fn relative_stem(&self) -> String {
        format!("{}/{}/{}", self.package, self.kind, self.type_name)
    }

    fn normalized_name(&self) -> String {
        self.relative_stem()
    }
}

fn parse_dds_type_name(name: &str) -> Option<(&str, &str, &str)> {
    let (package, remainder) = name.split_once("::")?;
    let (kind, remainder) = if let Some(rest) = remainder.strip_prefix("msg::dds_::") {
        ("msg", rest)
    } else if let Some(rest) = remainder.strip_prefix("srv::dds_::") {
        ("srv", rest)
    } else if let Some(rest) = remainder.strip_prefix("action::dds_::") {
        ("action", rest)
    } else {
        return None;
    };

    let type_name = remainder.strip_suffix('_').unwrap_or(remainder);
    Some((package, kind, type_name))
}

fn normalize_schema_name(name: &str) -> Cow<'_, str> {
    if name.contains('/') {
        return Cow::Borrowed(name);
    }

    ParsedTypeName::parse(name)
        .map(|parsed| Cow::Owned(parsed.normalized_name()))
        .unwrap_or(Cow::Borrowed(name))
}

// When rp runs as sudo, recorded files are owned by root.
// Restore ownership to the original user (SUDO_USER) so they can delete them.
fn restore_ownership(path: &Path) {
    let sudo_user = match env::var("SUDO_USER") {
        Ok(u) if !u.is_empty() => u,
        _ => return,
    };
    let Ok(cname) = CString::new(sudo_user) else {
        return;
    };
    unsafe {
        let pw = libc::getpwnam(cname.as_ptr());
        if pw.is_null() {
            return;
        }
        let uid = (*pw).pw_uid;
        let gid = (*pw).pw_gid;
        if let Ok(p) = CString::new(path.as_os_str().as_encoded_bytes()) {
            libc::chown(p.as_ptr(), uid, gid);
        }
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            if let Ok(p) = CString::new(parent.as_os_str().as_encoded_bytes()) {
                libc::chown(p.as_ptr(), uid, gid);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use serde_yaml::Value;

    use super::*;

    #[test]
    fn rosbag2_serialized_metadata_contains_bag_summary() {
        let topics = [TopicSummary {
            topic: "/chatter".to_string(),
            schema_name: "std_msgs/msg/String".to_string(),
            message_count: 3,
        }];

        let yaml = rosbag2_serialized_metadata(
            Path::new("/tmp/test.mcap"),
            Rosbag2MetadataVersion::V9,
            3,
            1_000,
            2_000,
            topics.iter(),
            "jazzy",
        )
        .unwrap();
        let value = serde_yaml::from_str::<Value>(&yaml).unwrap();

        assert_eq!(value["version"].as_i64(), Some(9));
        assert_eq!(value["storage_identifier"].as_str(), Some("mcap"));
        assert_eq!(value["message_count"].as_i64(), Some(3));
        assert_eq!(
            value["starting_time"]["nanoseconds_since_epoch"].as_i64(),
            Some(1_000)
        );
        assert_eq!(value["duration"]["nanoseconds"].as_i64(), Some(2_000));
        assert_eq!(value["relative_file_paths"][0].as_str(), Some("test.mcap"));
        assert_eq!(value["files"][0]["path"].as_str(), Some("test.mcap"));
        assert_eq!(value["files"][0]["message_count"].as_i64(), Some(3));

        let topic = &value["topics_with_message_count"][0];
        assert_eq!(topic["message_count"].as_i64(), Some(3));
        assert_eq!(topic["topic_metadata"]["name"].as_str(), Some("/chatter"));
        assert_eq!(
            topic["topic_metadata"]["type"].as_str(),
            Some("std_msgs/msg/String")
        );
        assert_eq!(
            topic["topic_metadata"]["serialization_format"].as_str(),
            Some("cdr")
        );
        assert!(
            topic["topic_metadata"]["offered_qos_profiles"]
                .as_sequence()
                .is_some_and(Vec::is_empty)
        );
        assert_eq!(
            topic["topic_metadata"]["type_description_hash"].as_str(),
            Some("")
        );
        assert_eq!(value["ros_distro"].as_str(), Some("jazzy"));
    }

    #[test]
    fn rosbag2_serialized_metadata_v5_uses_humble_compatible_fields() {
        let topics = [TopicSummary {
            topic: "/chatter".to_string(),
            schema_name: "std_msgs/msg/String".to_string(),
            message_count: 3,
        }];

        let yaml = rosbag2_serialized_metadata(
            Path::new("/tmp/test.mcap"),
            Rosbag2MetadataVersion::V5,
            3,
            1_000,
            2_000,
            topics.iter(),
            "humble",
        )
        .unwrap();
        let value = serde_yaml::from_str::<Value>(&yaml).unwrap();

        assert_eq!(value["version"].as_i64(), Some(5));
        let topic = &value["topics_with_message_count"][0];
        assert_eq!(
            topic["topic_metadata"]["offered_qos_profiles"].as_str(),
            Some("")
        );
        assert!(topic["topic_metadata"]["type_description_hash"].is_null());
        assert!(value["custom_data"].is_null());
        assert!(value["ros_distro"].is_null());
    }

    #[test]
    fn rosbag2_metadata_version_follows_ros_distro() {
        assert_eq!(
            rosbag2_metadata_version("humble"),
            Rosbag2MetadataVersion::V5
        );
        assert_eq!(
            rosbag2_metadata_version("jazzy"),
            Rosbag2MetadataVersion::V9
        );
        assert_eq!(
            rosbag2_metadata_version("unknown"),
            Rosbag2MetadataVersion::V5
        );
    }
}
