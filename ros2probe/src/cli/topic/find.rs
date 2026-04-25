use std::{
    io::{BufRead, BufReader, Write},
    os::unix::net::UnixStream,
};

use anyhow::{Context, bail};
use clap::Args;

use crate::command::{
    protocol::{CommandRequest, CommandResponse, TopicInfo, TopicListRequest},
    server::DEFAULT_COMMAND_SOCKET_PATH,
};

#[derive(Debug, Args)]
pub struct TopicFindCommand {
    /// Name of the ROS topic type to filter for
    pub topic_type: String,

    /// Only display the number of topics discovered
    #[arg(short = 'c', long = "count-topics")]
    pub count_only: bool,
}

pub fn run(args: TopicFindCommand) -> anyhow::Result<()> {
    let mut stream = UnixStream::connect(DEFAULT_COMMAND_SOCKET_PATH)
        .with_context(|| format!("connect command socket at {DEFAULT_COMMAND_SOCKET_PATH}"))?;
    let request = CommandRequest::TopicList(TopicListRequest {
        show_types: false,
        count_only: false,
        verbose: false,
        include_hidden: false,
    });
    serde_json::to_writer(&mut stream, &request).context("serialize request")?;
    stream.write_all(b"\n").context("write request newline")?;
    stream.flush().context("flush request")?;

    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    reader.read_line(&mut line).context("read response")?;
    let response: CommandResponse =
        serde_json::from_str(line.trim_end()).context("parse response")?;

    match response {
        CommandResponse::TopicList(response) => {
            let topics = response
                .topics
                .into_iter()
                .filter(|topic| matches_type(topic, &args.topic_type))
                .collect::<Vec<_>>();

            if args.count_only {
                println!("{}", topics.len());
                return Ok(());
            }

            for topic in topics {
                println!("{}", topic.name);
            }
            Ok(())
        }
        CommandResponse::Error(error) => bail!(error.message),
        _ => bail!("unexpected response for topic find request"),
    }
}

fn matches_type(topic: &TopicInfo, topic_type: &str) -> bool {
    topic.type_names.iter().any(|type_name| type_name == topic_type)
}
