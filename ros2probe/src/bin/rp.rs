use clap::{Parser, Subcommand};

use ros2probe::cli::action::info::ActionInfoCommand;
use ros2probe::cli::action::list::ActionListCommand;
use ros2probe::cli::bag::record::BagRecordCommand;
use ros2probe::cli::node::info::NodeInfoCommand;
use ros2probe::cli::node::list::NodeListCommand;
use ros2probe::cli::service::find::ServiceFindCommand;
use ros2probe::cli::service::list::ServiceListCommand;
use ros2probe::cli::service::r#type::ServiceTypeCommand;
use ros2probe::cli::discover::DiscoverCommand;
use ros2probe::cli::topic::{
    bw::TopicBwCommand,
    delay::TopicDelayCommand,
    echo::TopicEchoCommand,
    find::TopicFindCommand,
    hz::TopicHzCommand,
    info::TopicInfoCommand,
    list::TopicListCommand,
    r#type::TopicTypeCommand,
};

#[derive(Debug, Parser)]
#[command(name = "rp")]
#[command(about = "ros2probe command client")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Action introspection commands
    Action {
        #[command(subcommand)]
        command: ActionSubcommand,
    },
    /// Bag recording commands
    Bag {
        #[command(subcommand)]
        command: BagSubcommand,
    },
    /// Trigger spy discovery (forces /ros_discovery_info over UDP)
    Discover(DiscoverCommand),
    /// Launch the GUI
    #[cfg(feature = "gui")]
    Gui,
    /// Start the ros2probe runtime daemon
    Run,
    /// Node introspection commands
    Node {
        #[command(subcommand)]
        command: NodeSubcommand,
    },
    /// Topic introspection commands
    Topic {
        #[command(subcommand)]
        command: TopicSubcommand,
    },
    /// Service introspection commands
    Service {
        #[command(subcommand)]
        command: ServiceSubcommand,
    },
}

#[derive(Debug, Subcommand)]
enum ActionSubcommand {
    /// Print information about an action
    Info(ActionInfoCommand),
    /// Output a list of available actions
    List(ActionListCommand),
}

#[derive(Debug, Subcommand)]
enum BagSubcommand {
    /// Record ROS data to a bag
    Record(BagRecordCommand),
}

#[derive(Debug, Subcommand)]
enum NodeSubcommand {
    /// Print information about a node
    Info(NodeInfoCommand),
    /// Output a list of available nodes
    List(NodeListCommand),
}

#[derive(Debug, Subcommand)]
enum TopicSubcommand {
    /// Show the observed bandwidth for a topic
    Bw(TopicBwCommand),
    /// Display delay of topic from timestamp in header
    Delay(TopicDelayCommand),
    /// Output messages from a topic
    Echo(TopicEchoCommand),
    /// Output a list of available topics of a given type
    Find(TopicFindCommand),
    /// Show the observed publish rate for a topic
    Hz(TopicHzCommand),
    /// Print information about a topic
    Info(TopicInfoCommand),
    /// Output a list of available topics
    List(TopicListCommand),
    /// Output the type of a topic
    Type(TopicTypeCommand),
}

#[derive(Debug, Subcommand)]
enum ServiceSubcommand {
    /// Output a list of available services of a given type
    Find(ServiceFindCommand),
    /// Output a list of available services
    List(ServiceListCommand),
    /// Output the type of a service
    Type(ServiceTypeCommand),
}

fn is_interrupted_syscall(err: &anyhow::Error) -> bool {
    err.chain().any(|cause| {
        cause
            .downcast_ref::<std::io::Error>()
            .is_some_and(|io_err| {
                io_err.kind() == std::io::ErrorKind::Interrupted
                    || io_err.raw_os_error() == Some(libc::EINTR)
            })
            || cause.to_string().contains("Interrupted system call")
            || cause.to_string().contains("os error 4")
    })
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Action {
            command: ActionSubcommand::Info(args),
        } => ros2probe::cli::action::info::run(args),
        Command::Discover(args) => ros2probe::cli::discover::run(args),
        Command::Run => {
            // SAFETY: getuid() is always safe to call.
            if unsafe { libc::getuid() } != 0 {
                let exe = std::env::current_exe()?;
                let status = std::process::Command::new("sudo")
                    .arg("-E")
                    .arg(&exe)
                    .arg("run")
                    .status()?;
                return if status.success() {
                    Ok(())
                } else {
                    Err(anyhow::anyhow!("sudo rp run exited with {status}"))
                };
            }
            env_logger::init();
            let rt = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()?;
            println!("ros2probe started");
            let result = rt.block_on(ros2probe::runtime::run(
                ros2probe::runtime::RuntimeConfig::default(),
            ));
            match result {
                Ok(()) => {
                    println!("\nros2probe stopped");
                    Ok(())
                }
                Err(err) if is_interrupted_syscall(&err) => {
                    println!("\nros2probe stopped");
                    Ok(())
                }
                Err(err) => Err(err),
            }
        }
        #[cfg(feature = "gui")]
        Command::Gui => {
            env_logger::init();
            eframe::run_native(
                "ros2probe",
                eframe::NativeOptions::default(),
                Box::new(|_cc| Ok(Box::<ros2probe::gui::app::Ros2ProbeGuiApp>::default())),
            )
            .map_err(|err| anyhow::anyhow!("run gui: {err}"))
        }
        Command::Action {
            command: ActionSubcommand::List(args),
        } => ros2probe::cli::action::list::run(args),
        Command::Bag {
            command: BagSubcommand::Record(args),
        } => ros2probe::cli::bag::record::run(args),
        Command::Node {
            command: NodeSubcommand::Info(args),
        } => ros2probe::cli::node::info::run(args),
        Command::Node {
            command: NodeSubcommand::List(args),
        } => ros2probe::cli::node::list::run(args),
        Command::Topic {
            command: TopicSubcommand::Bw(args),
        } => ros2probe::cli::topic::bw::run(args),
        Command::Topic {
            command: TopicSubcommand::Delay(args),
        } => ros2probe::cli::topic::delay::run(args),
        Command::Topic {
            command: TopicSubcommand::Echo(args),
        } => ros2probe::cli::topic::echo::run(args),
        Command::Topic {
            command: TopicSubcommand::Find(args),
        } => ros2probe::cli::topic::find::run(args),
        Command::Topic {
            command: TopicSubcommand::Hz(args),
        } => ros2probe::cli::topic::hz::run(args),
        Command::Topic {
            command: TopicSubcommand::Info(args),
        } => ros2probe::cli::topic::info::run(args),
        Command::Topic {
            command: TopicSubcommand::List(args),
        } => ros2probe::cli::topic::list::run(args),
        Command::Topic {
            command: TopicSubcommand::Type(args),
        } => ros2probe::cli::topic::r#type::run(args),
        Command::Service {
            command: ServiceSubcommand::Find(args),
        } => ros2probe::cli::service::find::run(args),
        Command::Service {
            command: ServiceSubcommand::List(args),
        } => ros2probe::cli::service::list::run(args),
        Command::Service {
            command: ServiceSubcommand::Type(args),
        } => ros2probe::cli::service::r#type::run(args),
    }
}
