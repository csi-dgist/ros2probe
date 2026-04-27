use anyhow::bail;
use clap::Args;

use crate::{
    client::send_request,
    command::protocol::{CommandRequest, CommandResponse, TopicInfo, TopicListRequest},
};

#[derive(Debug, Args)]
pub struct TopicListCommand {
    /// Additionally show the topic type
    #[arg(short = 't', long = "show-types")]
    pub show_types: bool,

    /// Only display the number of topics discovered
    #[arg(short = 'c', long = "count-topics")]
    pub count_only: bool,

    /// List full details about each topic
    #[arg(short = 'v', long = "verbose")]
    pub verbose: bool,
}

pub fn run(args: TopicListCommand) -> anyhow::Result<()> {
    let response = send_request(CommandRequest::TopicList(TopicListRequest {
        show_types: args.show_types,
        count_only: args.count_only,
        verbose: args.verbose,
    }))?;

    match response {
        CommandResponse::TopicList(response) => {
            if args.count_only {
                println!("{}", response.total_count);
                return Ok(());
            }

            if args.verbose {
                print_verbose_topics(&response.topics);
                return Ok(());
            }

            let mut external: Vec<&TopicInfo> = Vec::new();
            let mut internal: Vec<&TopicInfo> = Vec::new();
            for topic in &response.topics {
                if topic.local_only {
                    internal.push(topic);
                } else {
                    external.push(topic);
                }
            }

            external.sort_by(|a, b| a.name.cmp(&b.name));
            internal.sort_by(|a, b| a.name.cmp(&b.name));

            for topic in &external {
                if args.show_types {
                    println!("{} [{}]", topic.name, format_types(&topic.type_names));
                } else {
                    println!("{}", topic.name);
                }
            }

            if !internal.is_empty() {
                println!();
                println!("Internal topics:");
                for topic in &internal {
                    if args.show_types {
                        println!("  {} [{}]", topic.name, format_types(&topic.type_names));
                    } else {
                        println!("  {}", topic.name);
                    }
                }
            }

            Ok(())
        }
        CommandResponse::Error(error) => bail!(error.message),
        _ => bail!("unexpected response for topic list request"),
    }
}

fn print_verbose_topics(topics: &[TopicInfo]) {
    println!("Published topics:");
    for topic in topics.iter().filter(|topic| topic.publisher_count > 0) {
        println!(
            " * {} [{}] {} {}",
            topic.name,
            format_types(&topic.type_names),
            topic.publisher_count,
            pluralize(topic.publisher_count, "publisher", "publishers"),
        );
    }
    println!();
    println!("Subscribed topics:");
    for topic in topics.iter().filter(|topic| topic.subscription_count > 0) {
        println!(
            " * {} [{}] {} {}",
            topic.name,
            format_types(&topic.type_names),
            topic.subscription_count,
            pluralize(topic.subscription_count, "subscriber", "subscribers"),
        );
    }
}

fn format_types(type_names: &[String]) -> String {
    if type_names.is_empty() {
        String::from("-")
    } else {
        type_names.join(", ")
    }
}

fn pluralize<'a>(count: usize, singular: &'a str, plural: &'a str) -> &'a str {
    if count == 1 { singular } else { plural }
}
