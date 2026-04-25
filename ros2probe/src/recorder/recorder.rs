use std::{
    collections::{BTreeMap, HashMap},
    env,
    fs::{self, File},
    io::BufWriter,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, bail};
use mcap::{
    records::{MessageHeader, Metadata},
    Compression, WriteOptions, Writer,
};

use crate::{
    protocols::rtps::RtpsDataMessage,
    recorder::gid_map::RecorderTopicMetadata,
    runtime::CompressionConfig,
};

#[derive(Clone, Debug, Eq, PartialEq, Hash)]
struct ChannelKey {
    topic: String,
    schema_name: String,
    qos_profile: String,
}

pub struct Recorder {
    path: PathBuf,
    writer: Writer<BufWriter<File>>,
    schema_ids: HashMap<String, u16>,
    channel_ids: HashMap<ChannelKey, u16>,
    next_sequence: HashMap<u16, u32>,
    ament_prefixes: Vec<PathBuf>,
}

impl Recorder {
    pub(crate) fn create(
        path: impl AsRef<Path>,
        compression: CompressionConfig,
    ) -> anyhow::Result<Self> {
        let path = path.as_ref().to_path_buf();
        if let Some(parent) = path.parent().filter(|parent| !parent.as_os_str().is_empty()) {
            fs::create_dir_all(parent)
                .with_context(|| format!("create output directory {}", parent.display()))?;
        }
        let file = File::create(&path)
            .with_context(|| format!("create MCAP file {}", path.display()))?;
        let mut writer = WriteOptions::new()
            .profile("ros2")
            .library("ros2probe")
            .compression(match compression {
                CompressionConfig::None => None,
                CompressionConfig::Zstd => Some(Compression::Zstd),
                CompressionConfig::Lz4 => Some(Compression::Lz4),
            })
            .create(BufWriter::new(file))
            .context("create MCAP writer")?;
        writer
            .write_metadata(&rosbag2_metadata())
            .context("write rosbag2 MCAP metadata")?;

        Ok(Self {
            path,
            writer,
            schema_ids: HashMap::new(),
            channel_ids: HashMap::new(),
            next_sequence: HashMap::new(),
            ament_prefixes: ament_prefixes(),
        })
    }

    pub fn write_rtps_data_message(
        &mut self,
        message: &RtpsDataMessage,
        metadata: &RecorderTopicMetadata,
    ) -> anyhow::Result<()> {
        self.write_message(
            message.captured_at,
            &message.payload,
            &metadata.topic_name,
            metadata.type_name.as_deref().unwrap_or("unknown/Unknown"),
            "",
        )
    }

    pub fn finish(mut self) -> anyhow::Result<PathBuf> {
        self.writer.finish().context("finish MCAP writer")?;
        Ok(self.path)
    }

    fn write_message(
        &mut self,
        captured_at: SystemTime,
        payload: &[u8],
        topic_name: &str,
        schema_name: &str,
        qos_profile: &str,
    ) -> anyhow::Result<()> {
        let normalized_schema_name = normalize_schema_name(schema_name);
        let schema_id = if let Some(schema_id) = self.schema_ids.get(&normalized_schema_name).copied() {
            schema_id
        } else {
            let resolved = resolve_schema(&normalized_schema_name, &self.ament_prefixes)
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
                .insert(normalized_schema_name.clone(), schema_id);
            schema_id
        };

        let channel_key = ChannelKey {
            topic: topic_name.to_string(),
            schema_name: normalized_schema_name.clone(),
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
            channel_id
        };

        let sequence = self.next_sequence.entry(channel_id).or_insert(0);
        let timestamp = system_time_to_nanos(captured_at)?;
        self.writer
            .write_to_known_channel(
                &MessageHeader {
                    channel_id,
                    sequence: *sequence,
                    log_time: timestamp,
                    publish_time: timestamp,
                },
                payload,
            )
            .context("write MCAP message")?;
        *sequence = sequence.saturating_add(1);
        Ok(())
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

fn normalize_schema_name(name: &str) -> String {
    ParsedTypeName::parse(name)
        .map(|parsed| parsed.normalized_name())
        .unwrap_or_else(|_| name.to_string())
}

fn rosbag2_metadata() -> Metadata {
    let mut metadata = BTreeMap::new();
    metadata.insert(
        String::from("ROS_DISTRO"),
        env::var("ROS_DISTRO").unwrap_or_else(|_| String::from("unknown")),
    );
    Metadata {
        name: String::from("rosbag2"),
        metadata,
    }
}

fn system_time_to_nanos(timestamp: SystemTime) -> anyhow::Result<u64> {
    let duration = timestamp
        .duration_since(UNIX_EPOCH)
        .context("timestamp earlier than UNIX_EPOCH")?;
    u64::try_from(duration.as_nanos()).context("timestamp does not fit in u64 nanoseconds")
}
