use anyhow::bail;
use clap::Args;

use crate::{
    client::send_request,
    command::protocol::{
        CommandRequest, CommandResponse, TopicDetails, TopicInfo, TopicInfoRequest, TopicListRequest,
    },
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

    /// Include hidden topics (e.g. action sub-topics under /_action/)
    #[arg(short = 'a', long = "include-hidden-topics")]
    pub include_hidden: bool,
}

pub fn run(args: TopicListCommand) -> anyhow::Result<()> {
    let response = send_request(CommandRequest::TopicList(TopicListRequest {
        show_types: args.show_types,
        count_only: args.count_only,
        verbose: args.verbose,
        include_hidden: args.include_hidden,
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

            // Split into: needs detail fetch (passed name+subscriber filter) vs already internal
            let mut needs_fetch: Vec<&TopicInfo> = Vec::new();
            let mut internal: Vec<&TopicInfo> = Vec::new();
            for topic in &response.topics {
                if passes_name_filter(&topic.name) && topic.subscription_count > 0 {
                    needs_fetch.push(topic);
                } else {
                    internal.push(topic);
                }
            }

            // Fetch local_only for candidates in parallel
            let handles: Vec<_> = needs_fetch
                .iter()
                .map(|t| {
                    let name = t.name.clone();
                    std::thread::spawn(move || fetch_topic_details(&name))
                })
                .collect();

            let mut recordable: Vec<&TopicInfo> = Vec::new();
            for (topic, handle) in needs_fetch.iter().zip(handles) {
                let details = handle.join().ok().flatten();
                let local_only = details.as_ref().map(|d| d.local_only).unwrap_or(false);
                if local_only {
                    internal.push(topic);
                } else {
                    recordable.push(topic);
                }
            }

            // internal may be out of order after appending; sort both by name
            recordable.sort_by(|a, b| a.name.cmp(&b.name));
            internal.sort_by(|a, b| a.name.cmp(&b.name));

            for topic in &recordable {
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

fn passes_name_filter(name: &str) -> bool {
    if name == "/tf" || name == "/tf_static" {
        return false;
    }
    if name.ends_with("/parameter_events") || name == "/rosout" {
        return false;
    }
    if name.split('/').any(|seg| !seg.is_empty() && seg.starts_with('_')) {
        return false;
    }
    true
}

fn fetch_topic_details(topic_name: &str) -> Option<TopicDetails> {
    match send_request(CommandRequest::TopicInfo(TopicInfoRequest {
        topic_name: topic_name.to_string(),
    })) {
        Ok(CommandResponse::TopicInfo(r)) => r.topic,
        _ => None,
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
